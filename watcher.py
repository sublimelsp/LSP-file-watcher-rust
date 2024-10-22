from hashlib import md5
from json import dumps
from LSP.plugin import FileWatcher
from LSP.plugin import FileWatcherEvent
from LSP.plugin import FileWatcherEventType
from LSP.plugin import FileWatcherProtocol
from LSP.plugin import register_file_watcher_implementation
from LSP.plugin.core.transports import AbstractProcessor
from LSP.plugin.core.transports import ProcessTransport
from LSP.plugin.core.transports import StopLoopError
from LSP.plugin.core.transports import Transport
from LSP.plugin.core.transports import TransportCallbacks
from LSP.plugin.core.typing import Any, Callable, cast, Dict, IO, List, Optional, Tuple
from os import makedirs
from os import path
from os import remove
from shutil import rmtree
from sublime_lib import ActivityIndicator
from sublime_lib import ResourcePath
import sublime
import subprocess
import weakref

platform = sublime.platform()
CHOKIDAR_CLI_PATH = path.join(path.dirname(__file__), '{}-{}'.format(platform,
                              'universal2' if platform == 'osx' else sublime.arch()), 'chokidar')

Uid = str


def log(message: str) -> None:
    print('{}: {}'.format(__package__, message))


class TemporaryInstallationMarker:
    """
    Creates a temporary file for the duration of the context.
    The temporary file is not removed if an exception triggeres within the context.

    Usage:

    ```
    with TemporaryInstallationMarker('/foo/file'):
        ...
    ```
    """

    def __init__(self, marker_path: str) -> None:
        self._marker_path = marker_path

    def __enter__(self) -> 'TemporaryInstallationMarker':
        makedirs(path.dirname(self._marker_path), exist_ok=True)
        open(self._marker_path, 'a').close()
        return self

    def __exit__(self, exc_type: Any, exc_value: Any, traceback: Any) -> None:
        if exc_type:
            # Don't remove the marker on exception.
            return
        remove(self._marker_path)


class StringTransportHandler(AbstractProcessor[str]):

    def write_data(self, writer: IO[bytes], data: str) -> None:
        writer.write('{}\n'.format(data).encode('utf-8'))

    def read_data(self, reader: IO[bytes]) -> Optional[str]:
        data = reader.readline()
        text = None
        try:
            text = data.decode('utf-8').strip()
        except Exception as ex:
            log("decode error: {}".format(ex))
        if not text:
            raise StopLoopError()
        return text


class FileWatcherController(FileWatcher):

    @classmethod
    def create(
        cls,
        root_path: str,
        patterns: List[str],
        events: List[FileWatcherEventType],
        ignores: List[str],
        handler: FileWatcherProtocol
    ) -> 'FileWatcher':
        return file_watcher.register_watcher(root_path, patterns, events, ignores, handler)

    def __init__(self, on_destroy: Callable[[], None]) -> None:
        self._on_destroy = on_destroy

    def destroy(self) -> None:
        self._on_destroy()


class FileWatcherChokidar(TransportCallbacks):

    def __init__(self) -> None:
        self._last_controller_id = 0
        self._handlers = {}  # type: Dict[str, Tuple[weakref.ref[FileWatcherProtocol], str]]
        self._transport = None  # type: Optional[Transport[str]]
        self._pending_events = {}  # type: Dict[Uid, List[FileWatcherEvent]]

    def register_watcher(
        self,
        root_path: str,
        patterns: List[str],
        events: List[FileWatcherEventType],
        ignores: List[str],
        handler: FileWatcherProtocol
    ) -> 'FileWatcherController':
        self._last_controller_id += 1
        controller_id = self._last_controller_id
        controller = FileWatcherController(on_destroy=lambda: self._on_watcher_removed(controller_id))
        self._on_watcher_added(controller_id, root_path, patterns, events, ignores, handler)
        return controller

    def _on_watcher_added(
        self,
        controller_id: int,
        root_path: str,
        patterns: List[str],
        events: List[FileWatcherEventType],
        ignores: List[str],
        handler: FileWatcherProtocol
    ) -> None:
        self._handlers[str(controller_id)] = (weakref.ref(handler), root_path)
        if len(self._handlers) and not self._transport:
            self._start_process()
        if not self._transport:
            log('ERROR: Failed creating transport')
            return
        # log('Starting watcher for directory "{}". Pattern: {}. Ignores: {}'.format(root_path, patterns, ignores))
        register_data = {
            'register': {
                'cwd': root_path,
                'events': events,
                'ignores': ignores,
                'patterns': patterns,
                'uid': controller_id,
            }
        }
        self._transport.send(self._to_json(register_data))

    def _on_watcher_removed(self, controller_id: int) -> None:
        # log('Removing watcher with id "{}"'.format(controller_id))
        self._handlers.pop(str(controller_id))
        if not self._transport:
            log('ERROR: Transport does not exist')
            return
        self._transport.send(self._to_json({'unregister': controller_id}))
        if not len(self._handlers) and self._transport:
            self._end_process()

    def _to_json(self, obj: Any) -> str:
        return dumps(
            obj,
            ensure_ascii=False,
            sort_keys=False,
            check_circular=False,
            separators=(',', ':')
        )

    def _start_process(self) -> None:
        # log('Starting watcher process')
        process = subprocess.Popen(
            [CHOKIDAR_CLI_PATH], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        if not process or not process.stdin or not process.stdout:
            raise RuntimeError('Failed initializing watcher process')
        self._transport = ProcessTransport(
            'lspwatcher', process, None, process.stdout, process.stdin, process.stderr, StringTransportHandler(), self)

    def _end_process(self, exception: Optional[Exception] = None) -> None:
        if self._transport:
            self._transport.close()
            self._transport = None
            log('Watcher process ended. Exception: {}'.format(str(exception)))

    # --- TransportCallbacks -------------------------------------------------------------------------------------------

    def on_payload(self, payload: str) -> None:
        # Chokidar debounces the events and sends them in batches but Transport notifies us for each new line
        # separately so we don't get the benefit of batching by default. To optimize the `on_file_event_async`
        # notifications we'll batch the events on our side and only notify when chokidar reports end of the batch
        # using the `<flush>` line.
        if payload == '<flush>':
            for uid, events in self._pending_events.items():
                handler, root_path = self._handlers[uid]
                handler_impl = handler()
                if not handler_impl:
                    log('ERROR: on_payload(): Handler already deleted')
                    continue
                handler_impl.on_file_event_async(events)
            self._pending_events.clear()
            return
        if ':' not in payload:
            log('Invalid watcher output: {}'.format(payload))
            return
        # Queue event.
        uid, event_type, cwd_relative_path = payload.split(':', 2)
        if uid not in self._pending_events:
            self._pending_events[uid] = []
        _, root_path = self._handlers[uid]
        event_kind = cast(FileWatcherEventType, event_type)
        log(str((event_kind, path.join(root_path, cwd_relative_path))))
        self._pending_events[uid].append((event_kind, path.join(root_path, cwd_relative_path)))

    def on_stderr_message(self, message: str) -> None:
        log('ERROR: {}'.format(message))

    def on_transport_close(self, exit_code: int, exception: Optional[Exception]) -> None:
        self._end_process(exception)


file_watcher = FileWatcherChokidar()

register_file_watcher_implementation(FileWatcherController)

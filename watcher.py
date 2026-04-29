from __future__ import annotations

from .transports import AbstractProcessor
from .transports import ProcessTransport
from .transports import StopLoopError
from .transports import Transport
from .transports import TransportCallbacks
from json import dumps
from LSP.plugin import FileWatcher
from LSP.plugin import FileWatcherEvent
from LSP.plugin import FileWatcherEventType
from LSP.plugin import FileWatcherProtocol
from LSP.plugin import register_file_watcher_implementation
from pathlib import Path
from typing import Callable
from typing import cast
from typing import IO
from typing import Protocol
import sublime
import subprocess
import weakref

platform = sublime.platform()
binary_name = '{}-{}'.format(platform, 'universal2' if platform == 'osx' else sublime.arch())
RUST_WATCHER_CLI_PATH = (Path(__file__).parent / binary_name / 'rust-watcher')

Uid = str


def log(message: str) -> None:
    print(f'{__package__}: {message}')


class StringTransportHandler(AbstractProcessor[str]):

    def write_data(self, writer: IO[bytes], data: str) -> None:
        writer.write(f'{data}\n'.encode())

    def read_data(self, reader: IO[bytes]) -> str | None:
        data = reader.readline()
        text = None
        try:
            text = data.decode('utf-8').strip()
        except Exception as ex:
            log(f"decode error: {ex}")
        if not text:
            raise StopLoopError
        return text


class EventCollector(Protocol):

    def on_events(self, uid: Uid, events: list[FileWatcherEvent]) -> None:
        pass


class ProcessHandler(TransportCallbacks[str]):
    def __init__(self, event_collector: EventCollector) -> None:
        self._transport: Transport[str] | None = None
        self._pending_events: dict[Uid, list[FileWatcherEvent]] = {}
        self._event_collector = event_collector
        self._start_process()

    def _start_process(self) -> None:
        # log('Starting watcher process')
        process = subprocess.Popen(
            [RUST_WATCHER_CLI_PATH], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        if not process or not process.stdin or not process.stdout:
            raise RuntimeError('Failed initializing watcher process')
        self._transport = ProcessTransport(
            'lspwatcher', process, None, process.stdout, process.stdin, process.stderr, StringTransportHandler(), self)

    def send(self, payload: str) -> None:
        if self._transport:
            self._transport.send(payload)

    def end_process(self, exit_code: int, exception: Exception | None = None) -> None:
        if self._transport:
            self._transport.close()
            self._transport = None
            if exit_code != 0:
                log(f'Watcher process ended. Exit code: {exit_code}, Exception: {exception}')

    # --- TransportCallbacks -------------------------------------------------------------------------------------------

    def on_payload(self, payload: str) -> None:
        # Watcher debounces the events and sends them in batches but Transport notifies us for each new line
        # separately so we don't get the benefit of batching by default. To optimize the `on_file_event_async`
        # notifications we'll batch the events on our side and only notify when watcher reports end of the batch
        # using the `<flush>` line.
        if payload == '<flush>':
            for uid, events in self._pending_events.items():
                self._event_collector.on_events(uid, events)
            self._pending_events.clear()
            return
        if ':' not in payload:
            log(f'Invalid watcher output: {payload}')
            return
        # Queue event.
        uid, event_type, path = payload.split(':', 2)
        if uid not in self._pending_events:
            self._pending_events[uid] = []
        event_kind = cast('FileWatcherEventType', event_type)
        self._pending_events[uid].append((event_kind, path))

    def on_stderr_message(self, message: str) -> None:
        log(f'ERROR: {message}')

    def on_transport_close(self, exit_code: int, exception: Exception | None) -> None:
        self.end_process(exit_code, exception)


class FileWatcherController(FileWatcher):

    @classmethod
    def create(
        cls,
        root_path: str,
        patterns: list[str],
        events: list[FileWatcherEventType],
        ignores: list[str],
        handler: FileWatcherProtocol
    ) -> FileWatcher:
        return file_watcher.register_watcher(root_path, patterns, events, ignores, handler)

    def __init__(self, on_destroy: Callable[[], None]) -> None:
        self._on_destroy = on_destroy

    def destroy(self) -> None:
        self._on_destroy()


class RustFileWatcher(EventCollector):

    def __init__(self) -> None:
        self._last_controller_id = 0
        self._handlers: dict[str, tuple[weakref.ref[FileWatcherProtocol], str]] = {}
        self._process_handler: ProcessHandler | None = None

    def register_watcher(
        self,
        root_path: str,
        patterns: list[str],
        events: list[FileWatcherEventType],
        ignores: list[str],
        handler: FileWatcherProtocol
    ) -> FileWatcherController:
        self._last_controller_id += 1
        controller_id = self._last_controller_id
        controller = FileWatcherController(on_destroy=lambda: self._on_watcher_removed(controller_id))
        self._on_watcher_added(controller_id, root_path, patterns, events, ignores, handler)
        return controller

    def _on_watcher_added(
        self,
        controller_id: int,
        root_path: str,
        patterns: list[str],
        events: list[FileWatcherEventType],
        ignores: list[str],
        handler: FileWatcherProtocol
    ) -> None:
        self._handlers[str(controller_id)] = (weakref.ref(handler), root_path)
        if not self._process_handler:
            self._process_handler = ProcessHandler(self)
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
        self._process_handler.send(self._to_json(register_data))

    def _on_watcher_removed(self, controller_id: int) -> None:
        # log('Removing watcher with id "{}"'.format(controller_id))
        self._handlers.pop(str(controller_id))
        if not self._process_handler:
            log('ERROR: Watcher process does not exist')
            return
        self._process_handler.send(self._to_json({'unregister': controller_id}))
        if not len(self._handlers) and self._process_handler:
            self._process_handler.end_process(0)
            self._process_handler = None

    def _to_json(self, obj: object) -> str:
        return dumps(
            obj,
            ensure_ascii=False,
            sort_keys=False,
            check_circular=False,
            separators=(',', ':')
        )

    # --- EventCollector -----------------------------------------------------------------------------------------------

    def on_events(self, uid: Uid, events: list[FileWatcherEvent]) -> None:
        if uid not in self._handlers:
            return
        handler, root_path = self._handlers[uid]
        handler_impl = handler()
        if not handler_impl:
            log('ERROR: on_payload(): Handler already deleted')
            return
        handler_impl.on_file_event_async([(e_type, str(Path(root_path, e_path))) for (e_type, e_path) in events])


file_watcher = RustFileWatcher()

register_file_watcher_implementation(FileWatcherController)

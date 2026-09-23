"""Durable, checksummed, append-only newline-delimited JSON journal."""

from __future__ import annotations

import contextlib
import copy
import errno
import fcntl
import hashlib
import json
import math
import os
import re
import stat
import time
import uuid
from pathlib import Path
from typing import Any, Iterator


_ZERO_HASH = "0" * 64
_HASH_RE = re.compile(r"[0-9a-f]{64}\Z")
_FIELDS = {"version", "stream", "seq", "prev", "payload", "sha256"}


class JournalError(Exception):
    """Base class for journal and protocol failures."""


class JournalBusy(JournalError):
    """Raised when the journal lock cannot be acquired before the timeout."""


class JournalCorrupt(JournalError):
    """Raised when committed journal history fails validation."""


class _DuplicateKey(ValueError):
    pass


def _pairs_without_duplicates(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise _DuplicateKey(f"duplicate JSON key: {key!r}")
        result[key] = value
    return result


def _reject_constant(value: str) -> None:
    raise ValueError(f"non-finite JSON number: {value}")


def _loads_strict(data: bytes) -> Any:
    try:
        text = data.decode("utf-8", errors="strict")
        return json.loads(
            text,
            object_pairs_hook=_pairs_without_duplicates,
            parse_constant=_reject_constant,
        )
    except (UnicodeDecodeError, json.JSONDecodeError, ValueError, RecursionError) as exc:
        raise JournalCorrupt(f"invalid JSON record: {exc}") from exc


def _validate_json_value(value: Any, *, where: str = "payload") -> None:
    if value is None or isinstance(value, (str, bool)):
        return
    if type(value) is int:
        return
    if type(value) is float:
        if not math.isfinite(value):
            raise JournalError(f"{where} contains a non-finite number")
        return
    if type(value) is list:
        for index, item in enumerate(value):
            _validate_json_value(item, where=f"{where}[{index}]")
        return
    if type(value) is dict:
        for key, item in value.items():
            if type(key) is not str:
                raise JournalError(f"{where} contains a non-string object key")
            _validate_json_value(item, where=f"{where}.{key}")
        return
    raise JournalError(f"{where} contains a non-JSON value of type {type(value).__name__}")


def _canonical(value: Any) -> bytes:
    try:
        return json.dumps(
            value,
            ensure_ascii=False,
            allow_nan=False,
            sort_keys=True,
            separators=(",", ":"),
        ).encode("utf-8")
    except (TypeError, ValueError, UnicodeEncodeError, RecursionError) as exc:
        raise JournalError(f"value cannot be encoded as canonical JSON: {exc}") from exc


def _hash_envelope(envelope: dict[str, Any]) -> str:
    unsigned = {key: value for key, value in envelope.items() if key != "sha256"}
    return hashlib.sha256(_canonical(unsigned)).hexdigest()


def _fsync_directory(path: Path) -> None:
    flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_CLOEXEC", 0)
    fd = os.open(path, flags)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def _ensure_directory(path: Path) -> None:
    if path.exists():
        if not path.is_dir():
            raise JournalError(f"journal parent is not a directory: {path}")
        return
    parent = path.parent
    if parent == path:
        raise JournalError(f"cannot create journal directory: {path}")
    _ensure_directory(parent)
    try:
        os.mkdir(path, 0o700)
    except FileExistsError:
        if not path.is_dir():
            raise JournalError(f"journal parent is not a directory: {path}")
    else:
        _fsync_directory(parent)


def _open_private_regular(path: Path, flags: int) -> tuple[int, bool]:
    common = getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
    created = False
    try:
        fd = os.open(path, flags | common | os.O_CREAT | os.O_EXCL, 0o600)
        created = True
    except FileExistsError:
        fd = os.open(path, flags | common)
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode):
            raise JournalError(f"journal path is not a regular file: {path}")
        os.fchmod(fd, 0o600)
        if created:
            os.fsync(fd)
            _fsync_directory(path.parent)
        return fd, created
    except BaseException:
        os.close(fd)
        raise


def _write_all(fd: int, data: bytes) -> None:
    view = memoryview(data)
    while view:
        try:
            written = os.write(fd, view)
        except InterruptedError:
            continue
        if written <= 0:
            raise JournalError("journal write made no progress")
        view = view[written:]


class LockedJournal:
    """A validated journal snapshot held under the journal's exclusive lock."""

    def __init__(
        self,
        *,
        path: Path,
        fd: int,
        records: list[dict[str, Any]],
        max_record: int,
    ) -> None:
        self.path = path
        self.records = records
        self._fd = fd
        self._max_record = max_record
        self._closed = False
        self._stream = records[0]["stream"] if records else None
        self._last_sequence = records[-1]["seq"] if records else 0
        self._previous_hash = records[-1]["sha256"] if records else _ZERO_HASH

    def append(self, payload: dict[str, Any]) -> dict[str, Any]:
        """Durably append one payload and return its checksummed envelope."""
        if self._closed:
            raise JournalError("locked journal is no longer active")
        if type(payload) is not dict:
            raise JournalError("journal payload must be a JSON object")
        _validate_json_value(payload)

        # Round-trip the payload so caller mutations cannot alter the validated snapshot.
        payload_copy = _loads_for_append(_canonical(payload))
        stream = self._stream or str(uuid.uuid4())
        sequence = self._last_sequence + 1
        previous = self._previous_hash

        envelope: dict[str, Any] = {
            "version": 1,
            "stream": stream,
            "seq": sequence,
            "prev": previous,
            "payload": payload_copy,
        }
        envelope["sha256"] = _hash_envelope(envelope)
        encoded = _canonical(envelope)
        if len(encoded) > self._max_record:
            raise JournalError(
                f"encoded journal record is {len(encoded)} bytes; maximum is {self._max_record}"
            )
        try:
            _write_all(self._fd, encoded + b"\n")
            os.fsync(self._fd)
        except OSError as exc:
            raise JournalError(f"could not durably append journal record: {exc}") from exc

        stored = copy.deepcopy(envelope)
        self.records.append(stored)
        self._stream = stream
        self._last_sequence = sequence
        self._previous_hash = stored["sha256"]
        return copy.deepcopy(stored)


def _loads_for_append(data: bytes) -> Any:
    """Decode canonical data whose validity was already checked by the caller."""
    try:
        return json.loads(data.decode("utf-8"), object_pairs_hook=_pairs_without_duplicates)
    except (UnicodeDecodeError, json.JSONDecodeError, ValueError) as exc:  # pragma: no cover
        raise JournalError(f"could not normalize payload: {exc}") from exc


class Journal:
    """A local append-only journal guarded by a stable sibling lock file."""

    def __init__(self, path: Path, timeout: float = 2.0, max_record: int = 1_048_576) -> None:
        self.path = Path(path)
        if (
            isinstance(timeout, bool)
            or not isinstance(timeout, (int, float))
            or not math.isfinite(timeout)
            or timeout < 0
        ):
            raise JournalError("journal lock timeout must be a finite non-negative number")
        if type(max_record) is not int or max_record <= 0:
            raise JournalError("maximum record size must be a positive integer")
        self.timeout = float(timeout)
        self.max_record = max_record
        self.lock_path = self.path.with_name(self.path.name + ".lock")

    @contextlib.contextmanager
    def locked(self) -> Iterator[LockedJournal]:
        """Lock, validate, and expose the journal until the context exits."""
        lock_fd: int | None = None
        journal_fd: int | None = None
        locked_journal: LockedJournal | None = None
        acquired = False
        try:
            _ensure_directory(self.path.parent)
            lock_fd, _ = _open_private_regular(self.lock_path, os.O_RDWR)
            self._acquire(lock_fd)
            acquired = True
            journal_fd, _ = _open_private_regular(self.path, os.O_RDWR | os.O_APPEND)
            records, incomplete_at = self._read_and_validate(journal_fd)
            if incomplete_at is not None:
                self._recover_tail(journal_fd, incomplete_at)
            locked_journal = LockedJournal(
                path=self.path,
                fd=journal_fd,
                records=records,
                max_record=self.max_record,
            )
            yield locked_journal
        except (JournalError, JournalCorrupt, JournalBusy):
            raise
        except OSError as exc:
            raise JournalError(f"journal operation failed: {exc}") from exc
        finally:
            if locked_journal is not None:
                locked_journal._closed = True
            if journal_fd is not None:
                os.close(journal_fd)
            if lock_fd is not None:
                if acquired:
                    try:
                        fcntl.flock(lock_fd, fcntl.LOCK_UN)
                    except OSError:
                        pass
                os.close(lock_fd)

    def _acquire(self, lock_fd: int) -> None:
        deadline = time.monotonic() + self.timeout
        while True:
            try:
                fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                return
            except OSError as exc:
                if exc.errno not in (errno.EACCES, errno.EAGAIN):
                    raise JournalError(f"could not lock journal: {exc}") from exc
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise JournalBusy(f"journal remained locked for {self.timeout:.3f} seconds")
                time.sleep(min(0.01, remaining))

    def _read_and_validate(self, fd: int) -> tuple[list[dict[str, Any]], int | None]:
        size = os.fstat(fd).st_size
        records: list[dict[str, Any]] = []
        offset = 0
        expected_stream: str | None = None
        expected_previous = _ZERO_HASH

        reader_fd = os.dup(fd)
        try:
            with os.fdopen(reader_fd, "rb", closefd=True) as reader:
                while offset < size:
                    reader.seek(offset)
                    line = reader.readline(self.max_record + 2)
                    if line.endswith(b"\n"):
                        raw = line[:-1]
                        if len(raw) > self.max_record:
                            raise JournalCorrupt(
                                f"committed record at byte {offset} exceeds {self.max_record} bytes"
                            )
                        record = self._validate_record(
                            raw,
                            expected_sequence=len(records) + 1,
                            expected_stream=expected_stream,
                            expected_previous=expected_previous,
                            offset=offset,
                        )
                        records.append(record)
                        expected_stream = record["stream"]
                        expected_previous = record["sha256"]
                        offset += len(line)
                        continue

                    # No newline within the bounded read. A later newline makes this a
                    # committed oversized record; EOF makes it an uncommitted tail.
                    scan_at = offset + len(line)
                    while scan_at < size:
                        chunk = os.pread(fd, min(65_536, size - scan_at), scan_at)
                        if not chunk:
                            break
                        if b"\n" in chunk:
                            raise JournalCorrupt(
                                f"committed record at byte {offset} exceeds {self.max_record} bytes"
                            )
                        scan_at += len(chunk)
                    return records, offset
        finally:
            # fdopen closes reader_fd on normal and exceptional paths after entry.
            pass
        return records, None

    def _validate_record(
        self,
        raw: bytes,
        *,
        expected_sequence: int,
        expected_stream: str | None,
        expected_previous: str,
        offset: int,
    ) -> dict[str, Any]:
        try:
            record = _loads_strict(raw)
            if type(record) is not dict or set(record) != _FIELDS:
                raise JournalCorrupt("record must be an object with the exact envelope fields")
            if type(record["version"]) is not int or record["version"] != 1:
                raise JournalCorrupt("unsupported journal record version")
            if type(record["stream"]) is not str:
                raise JournalCorrupt("record stream must be a UUID string")
            try:
                parsed_stream = uuid.UUID(record["stream"])
            except (ValueError, AttributeError) as exc:
                raise JournalCorrupt("record stream is not a UUID") from exc
            if str(parsed_stream) != record["stream"]:
                raise JournalCorrupt("record stream UUID is not canonical")
            if expected_stream is not None and record["stream"] != expected_stream:
                raise JournalCorrupt("record stream changed within the journal")
            if type(record["seq"]) is not int or record["seq"] != expected_sequence:
                raise JournalCorrupt(
                    f"record sequence is {record['seq']!r}; expected {expected_sequence}"
                )
            if type(record["prev"]) is not str or not _HASH_RE.fullmatch(record["prev"]):
                raise JournalCorrupt("record previous hash is malformed")
            if record["prev"] != expected_previous:
                raise JournalCorrupt("record previous hash does not match committed history")
            if type(record["payload"]) is not dict:
                raise JournalCorrupt("record payload must be a JSON object")
            _validate_json_value(record["payload"])
            if type(record["sha256"]) is not str or not _HASH_RE.fullmatch(record["sha256"]):
                raise JournalCorrupt("record checksum is malformed")
            calculated = _hash_envelope(record)
            if record["sha256"] != calculated:
                raise JournalCorrupt("record checksum does not match its contents")
            return record
        except JournalCorrupt as exc:
            raise JournalCorrupt(f"corrupt journal record at byte {offset}: {exc}") from exc
        except JournalError as exc:
            raise JournalCorrupt(f"corrupt journal record at byte {offset}: {exc}") from exc
        except RecursionError as exc:
            raise JournalCorrupt(
                f"corrupt journal record at byte {offset}: JSON nesting is too deep"
            ) from exc

    def _recover_tail(self, fd: int, tail_at: int) -> None:
        recoveries = self.path.parent / "recoveries"
        _ensure_directory(recoveries)
        recovery = recoveries / (
            f"{self.path.name}.tail-{time.time_ns()}-{os.getpid()}-{uuid.uuid4().hex}.bin"
        )
        flags = os.O_WRONLY | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
        recovery_fd = os.open(recovery, flags | os.O_CREAT | os.O_EXCL, 0o600)
        try:
            offset = tail_at
            size = os.fstat(fd).st_size
            while offset < size:
                chunk = os.pread(fd, min(65_536, size - offset), offset)
                if not chunk:
                    raise JournalError("journal tail changed while it was being recovered")
                _write_all(recovery_fd, chunk)
                offset += len(chunk)
            os.fsync(recovery_fd)
        finally:
            os.close(recovery_fd)
        _fsync_directory(recoveries)
        os.ftruncate(fd, tail_at)
        os.fsync(fd)


__all__ = ["Journal", "JournalBusy", "JournalCorrupt", "JournalError", "LockedJournal"]

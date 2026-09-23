from __future__ import annotations

import hashlib
import json
import multiprocessing
import os
import signal
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

from collab_core.journal import Journal, JournalBusy, JournalCorrupt, JournalError


def _append_many(path: str, worker: int, count: int) -> None:
    journal = Journal(Path(path), timeout=10.0)
    for index in range(count):
        with journal.locked() as locked:
            locked.append({"worker": worker, "index": index})


def _hold_lock(path: str, ready: multiprocessing.synchronize.Event, release: multiprocessing.synchronize.Event) -> None:
    with Journal(Path(path), timeout=5.0).locked():
        ready.set()
        release.wait(10.0)


def _write_partial_and_wait(path: str, partial: bytes, ready: multiprocessing.synchronize.Event) -> None:
    with Journal(Path(path), timeout=5.0).locked() as locked:
        os.write(locked._fd, partial)
        ready.set()
        time.sleep(30.0)


def _canonical(value: object) -> bytes:
    return json.dumps(
        value,
        ensure_ascii=False,
        allow_nan=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")


def _checksum(envelope: dict[str, object]) -> str:
    unsigned = {key: value for key, value in envelope.items() if key != "sha256"}
    return hashlib.sha256(_canonical(unsigned)).hexdigest()


class JournalTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.path = Path(self.temporary.name) / "nested" / "events.jsonl"

    def test_append_round_trip_and_chain(self) -> None:
        journal = Journal(self.path)
        with journal.locked() as locked:
            first = locked.append({"kind": "first", "text": "olá"})
            second = locked.append({"kind": "second"})

        self.assertEqual(first["version"], 1)
        self.assertEqual(first["seq"], 1)
        self.assertEqual(first["prev"], "0" * 64)
        self.assertEqual(second["seq"], 2)
        self.assertEqual(second["stream"], first["stream"])
        self.assertEqual(second["prev"], first["sha256"])
        self.assertEqual(self.path.read_bytes().count(b"\n"), 2)
        self.assertEqual(self.path.stat().st_mode & 0o777, 0o600)
        self.assertEqual(self.path.parent.stat().st_mode & 0o777, 0o700)
        self.assertEqual(journal.lock_path.stat().st_mode & 0o777, 0o600)

        with journal.locked() as locked:
            self.assertEqual(locked.records, [first, second])

    def test_concurrent_processes_append_without_loss(self) -> None:
        process_count = 6
        per_process = 20
        processes = [
            multiprocessing.Process(
                target=_append_many,
                args=(str(self.path), worker, per_process),
            )
            for worker in range(process_count)
        ]
        for process in processes:
            process.start()
        for process in processes:
            process.join(20.0)
            self.assertFalse(process.is_alive())
            self.assertEqual(process.exitcode, 0)

        with Journal(self.path).locked() as locked:
            self.assertEqual(len(locked.records), process_count * per_process)
            self.assertEqual(
                [record["seq"] for record in locked.records],
                list(range(1, process_count * per_process + 1)),
            )
            observed = {
                (record["payload"]["worker"], record["payload"]["index"])
                for record in locked.records
            }
        expected = {
            (worker, index)
            for worker in range(process_count)
            for index in range(per_process)
        }
        self.assertEqual(observed, expected)

    def test_killed_writer_partial_tail_is_preserved_and_repaired(self) -> None:
        with Journal(self.path).locked() as locked:
            committed = locked.append({"committed": True})
        committed_bytes = self.path.read_bytes()
        partial = b'{"payload":"interrupted"'
        ready = multiprocessing.Event()
        process = multiprocessing.Process(
            target=_write_partial_and_wait,
            args=(str(self.path), partial, ready),
        )
        process.start()
        self.assertTrue(ready.wait(5.0))
        os.kill(process.pid, signal.SIGKILL)
        process.join(5.0)
        self.assertEqual(process.exitcode, -signal.SIGKILL)
        self.assertEqual(self.path.read_bytes(), committed_bytes + partial)

        with Journal(self.path).locked() as locked:
            self.assertEqual(locked.records, [committed])
            appended = locked.append({"after": "recovery"})

        self.assertEqual(appended["seq"], 2)
        self.assertTrue(self.path.read_bytes().endswith(b"\n"))
        recoveries = list((self.path.parent / "recoveries").iterdir())
        self.assertEqual(len(recoveries), 1)
        self.assertEqual(recoveries[0].read_bytes(), partial)
        self.assertEqual(recoveries[0].stat().st_mode & 0o777, 0o600)

    def test_complete_corruption_fails_closed_and_leaves_file_unchanged(self) -> None:
        with Journal(self.path).locked() as locked:
            locked.append({"safe": True})
        raw = bytearray(self.path.read_bytes())
        marker = raw.index(b'"safe":true')
        raw[marker + len(b'"safe":')] = ord("f")
        raw.extend(b'{"later":"incomplete"')
        self.path.write_bytes(raw)
        before = self.path.read_bytes()

        with self.assertRaises(JournalCorrupt):
            with Journal(self.path).locked():
                pass

        self.assertEqual(self.path.read_bytes(), before)
        self.assertFalse((self.path.parent / "recoveries").exists())

    def test_chain_mismatch_is_rejected_even_with_valid_record_checksum(self) -> None:
        with Journal(self.path).locked() as locked:
            locked.append({"number": 1})
            locked.append({"number": 2})
        lines = self.path.read_bytes().splitlines()
        second = json.loads(lines[1])
        second["prev"] = "f" * 64
        second["sha256"] = _checksum(second)
        self.path.write_bytes(lines[0] + b"\n" + _canonical(second) + b"\n")

        with self.assertRaisesRegex(JournalCorrupt, "previous hash"):
            with Journal(self.path).locked():
                pass

    def test_invalid_complete_json_is_corruption_not_recovery(self) -> None:
        self.path.parent.mkdir(mode=0o700)
        invalid = b'{"not":"closed"\n'
        self.path.write_bytes(invalid)

        with self.assertRaises(JournalCorrupt):
            with Journal(self.path).locked():
                pass

        self.assertEqual(self.path.read_bytes(), invalid)
        self.assertFalse((self.path.parent / "recoveries").exists())

    def test_duplicate_keys_and_nonfinite_numbers_are_rejected(self) -> None:
        self.path.parent.mkdir(mode=0o700)
        duplicate = b'{"version":1,"version":1}\n'
        self.path.write_bytes(duplicate)
        with self.assertRaises(JournalCorrupt):
            with Journal(self.path).locked():
                pass

        self.path.write_bytes(b'{"value":NaN}\n')
        with self.assertRaises(JournalCorrupt):
            with Journal(self.path).locked():
                pass

        self.path.write_bytes(b"")
        with Journal(self.path).locked() as locked:
            with self.assertRaises(JournalError):
                locked.append({"value": float("nan")})
        self.assertEqual(self.path.read_bytes(), b"")

    def test_lock_timeout_raises_journal_busy(self) -> None:
        ready = multiprocessing.Event()
        release = multiprocessing.Event()
        process = multiprocessing.Process(
            target=_hold_lock,
            args=(str(self.path), ready, release),
        )
        process.start()
        self.addCleanup(lambda: process.kill() if process.is_alive() else None)
        self.assertTrue(ready.wait(5.0))
        started = time.monotonic()
        with self.assertRaises(JournalBusy):
            with Journal(self.path, timeout=0.05).locked():
                pass
        elapsed = time.monotonic() - started
        self.assertGreaterEqual(elapsed, 0.04)
        self.assertLess(elapsed, 1.0)
        release.set()
        process.join(5.0)
        self.assertEqual(process.exitcode, 0)

    def test_oversize_append_does_not_modify_journal(self) -> None:
        journal = Journal(self.path, max_record=1024)
        with journal.locked() as locked:
            locked.append({"small": True})
            before = self.path.read_bytes()
            with self.assertRaisesRegex(JournalError, "maximum"):
                locked.append({"large": "x" * 2000})
            self.assertEqual(locked.records[-1]["payload"], {"small": True})
        self.assertEqual(self.path.read_bytes(), before)

    def test_append_completes_short_os_writes_before_acknowledging(self) -> None:
        real_write = os.write
        write_sizes: list[int] = []

        def short_write(fd: int, data: bytes) -> int:
            limited = data[: max(1, len(data) // 3)]
            write_sizes.append(len(limited))
            return real_write(fd, limited)

        journal = Journal(self.path)
        with journal.locked() as locked:
            with mock.patch("collab_core.journal.os.write", side_effect=short_write):
                appended = locked.append({"text": "x" * 1000})
        self.assertGreater(len(write_sizes), 1)
        with journal.locked() as locked:
            self.assertEqual(locked.records, [appended])

    def test_public_record_mutation_cannot_change_append_chain_anchors(self) -> None:
        journal = Journal(self.path)
        with journal.locked() as locked:
            first = locked.append({"number": 1})
            locked.records[0]["seq"] = 999
            locked.records[0]["sha256"] = "f" * 64
            second = locked.append({"number": 2})
        self.assertEqual(second["seq"], 2)
        self.assertEqual(second["prev"], first["sha256"])
        with journal.locked() as locked:
            self.assertEqual([record["seq"] for record in locked.records], [1, 2])

    def test_symlink_log_and_lock_are_not_followed(self) -> None:
        self.path.parent.mkdir(mode=0o700)
        target = self.path.parent / "target"
        target.write_bytes(b"untouched")
        self.path.symlink_to(target)
        with self.assertRaises(JournalError):
            with Journal(self.path).locked():
                pass
        self.assertEqual(target.read_bytes(), b"untouched")

        self.path.unlink()
        lock_target = self.path.parent / "lock-target"
        lock_target.write_bytes(b"untouched-lock")
        lock_path = self.path.with_name(self.path.name + ".lock")
        lock_path.unlink()
        lock_path.symlink_to(lock_target)
        with self.assertRaises(JournalError):
            with Journal(self.path).locked():
                pass
        self.assertEqual(lock_target.read_bytes(), b"untouched-lock")


if __name__ == "__main__":
    unittest.main()

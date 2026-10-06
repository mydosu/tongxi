"""Shared provenance helpers for Stage 7 verification reports."""
import hashlib
from pathlib import Path


def sha256_file(path):
    path = Path(path)
    return hashlib.sha256(path.read_bytes()).hexdigest()


def begin_executable_run(path):
    path = Path(path).resolve()
    return {
        'executable': str(path),
        'executable_sha256_before': sha256_file(path),
    }


def finish_executable_run(path, provenance):
    path = Path(path).resolve()
    after = sha256_file(path)
    return {
        **provenance,
        'executable_sha256_after': after,
        'executable_unchanged_during_run': provenance['executable_sha256_before'] == after,
    }


def classify_report_provenance(report, current_sha256):
    before = report.get('executable_sha256_before')
    after = report.get('executable_sha256_after')
    if not before and not after:
        return 'legacy-unbound'
    if not before or not after or before != after or not report.get('executable_unchanged_during_run'):
        return 'invalid-provenance'
    if before != current_sha256:
        return 'different-candidate'
    return 'current-candidate'

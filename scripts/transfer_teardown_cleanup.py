"""Process cleanup used by the transfer teardown acceptance runner."""

import subprocess


def reap_process(process, log_stream):
    """Stop a child with bounded escalation and always close its log stream."""
    try:
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            process.terminate()
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=3)
    finally:
        log_stream.close()

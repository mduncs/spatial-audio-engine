#!/usr/bin/env python3
"""Bounded, window-free Workbench playback to BlackHole 2ch, with loopback.

Requires a previously built linked-sdk,live-output release Workbench. Never
changes the system output or falls back to another device. No microphone,
speakers, GUI or browser is opened. Use --start-audio to authorize this route.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import time

REPO = Path(__file__).resolve().parents[1]
DEVICE = 'BlackHole 2ch'


def stop_child(child):
    if child is not None and child.poll() is None:
        child.terminate()
        try:
            child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait(timeout=5)


def run_once(root, seconds):
    root.mkdir()
    recorder = app = None
    with (root / 'loopback.log').open('w') as record_log, \
            (root / 'workbench.log').open('w') as app_log:
        try:
            recorder = subprocess.Popen(
                ['ffmpeg', '-hide_banner', '-f', 'avfoundation',
                 '-i', ':' + DEVICE, '-t', str(seconds + 30),
                 '-c:a', 'pcm_f32le', str(root / 'blackhole.wav')],
                stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=record_log)
            time.sleep(0.5)
            if recorder.poll() is not None:
                raise RuntimeError('BlackHole input failed; see loopback.log. No playback started.')
            command = [str(REPO / 'target/release/fightbox-workbench'),
                       '--package', str(REPO.parent / 'evidence/megablock-seed1/megablock.fightbox'),
                       '--baked', str(REPO.parent / 'evidence/megablock-seed1/megablock.baked'),
                       '--fixture', str(REPO / 'fixtures/city/astra-corner/fixture.json'),
                       '--headless-replay', '--seconds', str(seconds),
                       '--capture-root', str(root / 'captures'),
                       '--start-audio', '--device', DEVICE]
            (root / 'command.json').write_text(json.dumps(command, indent=2) + '\n')
            app = subprocess.Popen(command, cwd=REPO, stdout=subprocess.PIPE,
                                   stderr=app_log, text=True)
            stdout, _ = app.communicate(timeout=seconds + 45)
            (root / 'workbench.stdout').write_text(stdout)
            time.sleep(0.25)
            if recorder.poll() is None:
                recorder.communicate(input=b'q\n', timeout=8)
            if app.returncode != 0:
                raise RuntimeError('Workbench replay failed; see workbench.log and capture sidecars')
            if recorder.returncode != 0:
                raise RuntimeError('BlackHole recording failed; see loopback.log')
            bundle = Path(stdout.strip())
            if not bundle.is_absolute() or not bundle.resolve().is_relative_to(root):
                raise RuntimeError('Workbench returned an invalid capture path')
            replay = json.loads((bundle / 'replay.json').read_text())
            if replay['actual_device'] != DEVICE or replay['status'] != 'ended_and_stopped':
                raise RuntimeError('Wrong device or incomplete replay')
            result = {'bundle': str(bundle), 'loopback': str(root / 'blackhole.wav'),
                      'device': DEVICE, 'workbench_returncode': app.returncode,
                      'recorder_returncode': recorder.returncode}
            (root / 'run.json').write_text(json.dumps(result, indent=2) + '\n')
            return result
        finally:
            # Only processes created by this invocation are ever stopped.
            stop_child(app)
            if recorder is not None and recorder.poll() is None:
                try:
                    recorder.communicate(input=b'q\n', timeout=5)
                except (subprocess.TimeoutExpired, BrokenPipeError, ValueError):
                    stop_child(recorder)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--start-audio', action='store_true')
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--seconds', type=int, default=20, choices=range(1, 61))
    p.add_argument('--repeat', type=int, default=2, choices=[1, 2])
    args = p.parse_args()
    if not args.start_audio:
        p.error('Explicit --start-audio is required; the only route is BlackHole 2ch')
    root = args.output.resolve()
    if root.is_relative_to(REPO) or root.exists():
        p.error('Use a fresh output directory outside the repository')
    root.mkdir(parents=True)
    binary = REPO / 'target/release/fightbox-workbench'
    with binary.open('rb') as stream:
        digest = hashlib.file_digest(stream, 'sha256').hexdigest()
    (root / 'binary.sha256').write_text(digest + '\n')
    results = []
    for i in range(args.repeat):
        print(f'Run {i + 1}/{args.repeat}: {args.seconds}s to {DEVICE}', flush=True)
        results.append(run_once(root / f'run-{i + 1}', args.seconds))
    (root / 'runs.json').write_text(json.dumps(results, indent=2) + '\n')
    print(json.dumps(results), flush=True)


if __name__ == '__main__':
    main()

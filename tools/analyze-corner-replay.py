#!/usr/bin/env python3
"""Make a silent, local replay report from Workbench capture + control trace.

No audio device is opened. ffmpeg is used only to decode files. Measurements
describe the captured program, not a perceptual acceptance or calibrated SPL.
"""
import argparse
import array
import hashlib
import json
import math
from pathlib import Path
import subprocess
import sys

RATE = 48000
REPO = Path(__file__).resolve().parents[1]


def read_json(path):
    return json.loads(Path(path).read_text())


def sha(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def decode(path):
    probe = read_probe(path)
    if int(probe['sample_rate']) != RATE or int(probe['channels']) != 2:
        raise ValueError('Expected an actual 48 kHz stereo capture; do not silently resample')
    result = subprocess.run(
        ['ffmpeg', '-v', 'error', '-i', str(path), '-map', '0:a:0',
         '-f', 'f32le', '-c:a', 'pcm_f32le', 'pipe:1'],
        check=True, capture_output=True)
    samples = array.array('f')
    samples.frombytes(result.stdout)
    if sys.byteorder != 'little':
        samples.byteswap()
    if not samples or len(samples) % 2 or not all(math.isfinite(x) for x in samples):
        raise ValueError('Empty, incomplete, or non-finite capture')
    return samples


def read_probe(path):
    result = subprocess.run(
        ['ffprobe', '-v', 'error', '-select_streams', 'a:0',
         '-show_entries', 'stream=sample_rate,channels', '-of', 'json', str(path)],
        check=True, capture_output=True, text=True)
    return json.loads(result.stdout)['streams'][0]


def db(amplitude):
    return max(-120.0, 20 * math.log10(max(amplitude, 1e-6)))


def windows(samples, hop=4800):
    # Two independent, first-order 250 Hz lowpasses. This is a descriptive
    # bass-body trace, not an octave-band filter or an acoustic transfer ratio.
    alpha = 1 - math.exp(-2 * math.pi * 250 / RATE)
    low = [0.0, 0.0]
    out = []
    for start in range(0, len(samples), hop * 2):
        count = min(hop * 2, len(samples) - start)
        energy = bass = peak = 0.0
        for i in range(start, start + count):
            x = samples[i]
            channel = i % 2
            low[channel] += alpha * (x - low[channel])
            energy += x * x
            bass += low[channel] ** 2
            peak = max(peak, abs(x))
        out.append({'time_s': start / (2 * RATE),
                    'rms_dbfs': db(math.sqrt(energy / count)),
                    'bass_dbfs': db(math.sqrt(bass / count)),
                    'peak_dbfs': db(peak)})
    return out


def energy_envelope(samples, hop=480):
    return [math.sqrt(sum(float(v) ** 2 for v in samples[i:i + hop * 2]) /
                      len(samples[i:i + hop * 2]))
            for i in range(0, len(samples), hop * 2)]


def loopback_match(reference, recording):
    """Find a constant capture-start offset; never time-warp to conceal drops."""
    if not any(recording):
        return {'matches_reference': False, 'error': 'loopback is entirely silent',
                'offset_frames': None, 'correlation': 0.0, 'relative_rms_error': 1.0}
    ref = energy_envelope(reference)
    rec = energy_envelope(recording)
    if len(rec) < len(ref):
        raise ValueError('Loopback is shorter than the Workbench reference')
    # Coarse delay fit on natural program energy at 10 ms resolution.
    probes = list(range(0, len(ref), 4))
    ref_norm = sum(ref[i] ** 2 for i in probes)
    if ref_norm == 0:
        raise ValueError('Reference is silent')
    def coarse(offset):
        dot = sum(ref[i] * rec[i + offset] for i in probes)
        norm = sum(rec[i + offset] ** 2 for i in probes)
        return dot / math.sqrt(max(1e-30, ref_norm * norm))
    delay_hops = max(range(len(rec) - len(ref) + 1), key=coarse)
    # Refine on sparse PCM samples, then measure across the whole capture.
    candidates = range(max(0, delay_hops * 480 - 600),
                       min((len(recording) - len(reference)) // 2,
                           delay_hops * 480 + 600) + 1)
    indices = list(range(0, len(reference), max(2, len(reference) // 1200 // 2 * 2)))
    def error(offset):
        return sum((reference[i] - recording[i + offset * 2]) ** 2 for i in indices)
    delay = min(candidates, key=error)
    error_energy = signal_energy = dot = captured_energy = 0.0
    max_error = 0.0
    for i, x in enumerate(reference):
        y = recording[i + delay * 2]
        e = x - y
        error_energy += e * e
        signal_energy += x * x
        captured_energy += y * y
        dot += x * y
        max_error = max(max_error, abs(e))
    ratio = math.sqrt(error_energy / max(signal_energy, 1e-30))
    return {'alignment': 'one constant offset; no resampling or time warp',
            'offset_frames': delay, 'offset_s': delay / RATE,
            'correlation': dot / math.sqrt(max(1e-30, signal_energy * captured_energy)),
            'relative_rms_error': ratio, 'maximum_sample_error': max_error,
            'compared_frames': len(reference) // 2,
            'matches_reference': ratio < 0.01}


def nearby_buildings(geojson, waypoints, source):
    points = [source, *waypoints]
    xmin = min(p[0] for p in points) - 15
    xmax = max(p[0] for p in points) + 15
    ymin = min(p[1] for p in points) - 15
    ymax = max(p[1] for p in points) + 15
    result = []
    for feature in geojson['features']:
        if feature['geometry']['type'] != 'Polygon':
            continue
        ring = feature['geometry']['coordinates'][0]
        if (max(p[0] for p in ring) < xmin or min(p[0] for p in ring) > xmax or
                max(p[1] for p in ring) < ymin or min(p[1] for p in ring) > ymax):
            continue
        identity = feature.get('id', feature['properties'].get('id', 'building'))
        result.append({'id': identity, 'ring': ring,
                       'label': 'Corner building' if identity == 'synth/1/1/0' else ''})
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--replay', type=Path, required=True)
    parser.add_argument('--capture', type=Path, required=True)
    parser.add_argument('--fixture', type=Path, required=True)
    parser.add_argument('--geojson', type=Path, required=True)
    parser.add_argument('--loopback', type=Path)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    out = args.output.resolve()
    if out.is_relative_to(REPO):
        parser.error('Report output must be outside the repository')
    if out.exists():
        parser.error('Use a fresh output directory; prior evidence is preserved')
    replay = read_json(args.replay)
    fixture = read_json(args.fixture)
    pcm = decode(args.capture)
    if max(abs(x) for x in pcm) == 0:
        raise ValueError('Capture is silent')
    audio_windows = windows(pcm)
    route = fixture['listener']['trajectory']
    source = fixture['sources'][0]
    block_seconds = replay['block_size'] / replay['sample_rate_hz']
    samples = []
    for row in replay['control_samples']:
        stage = row['live_stage_energy']
        known = row.get('acoustic_known', False)
        samples.append({'time_s': row['sampled_block'] * block_seconds,
                        'position_m': row['position_m'],
                        'occlusion': row['occlusion'] if known else None,
                        'path_strength': row['path_strength'] if known else None,
                        'direct_path_energy': stage['direct_path_energy'],
                        'reflection_energy': stage['reflection_energy']})
    if not samples:
        raise ValueError('No route samples')
    nonclaims = [
        'Illustrative waves are not solver rays or a measured acoustic field.',
        'Audio level/tone traces include the changing music; they are not source-normalized transfer measurements.',
        'Bass trace: first-order 250 Hz lowpass, 100 ms RMS; not calibrated octave-band or ear SPL.',
        'Direct and path energy are combined by existing telemetry; reflection energy is separate and pre-monitor.',
        'Control publication is bracketed by audio blocks, not sample-locked simulation adoption.',
        'No human realism judgment and no acoustic fix are claimed by this baseline.'
    ]
    data = {'title': 'One corner · Tom\u2019s Diner',
            'duration_s': len(pcm) / (2 * RATE),
            'source': {'position_m': source['position_m'], 'label': 'Tom\u2019s Diner'},
            'route': route, 'buildings': nearby_buildings(read_json(args.geojson),
                         route['waypoints_m'], source['position_m']),
            'samples': samples, 'audio_windows': audio_windows,
            'summary': {'text': 'Baseline capture. Mark a change; do not infer a pass from the plots.',
                        'device': replay['actual_device'], 'route_status': replay['status'],
                        'nonclaims': nonclaims},
            'artifacts': {name: {'path': str(path.resolve()), 'sha256': sha(path)}
                          for name, path in [('capture', args.capture), ('replay', args.replay),
                                             ('fixture', args.fixture), ('geojson', args.geojson)]}}
    if args.loopback:
        data['loopback'] = loopback_match(pcm, decode(args.loopback))
        data['artifacts']['loopback'] = {'path': str(args.loopback.resolve()),
                                         'sha256': sha(args.loopback)}
        data['summary']['verification'] = (
            'Engine capture + BlackHole loopback agree' if data['loopback']['matches_reference']
            else 'Engine capture available · BlackHole loopback NOT verified')
    known_samples = [s for s in samples if s['occlusion'] is not None]
    if not known_samples:
        raise ValueError('No known acoustic telemetry')
    data['measurements'] = {
        'peak_dbfs': max(w['peak_dbfs'] for w in audio_windows),
        'occlusion_min': min(s['occlusion'] for s in known_samples),
        'occlusion_max': max(s['occlusion'] for s in known_samples),
        'path_strength_min': min(s['path_strength'] for s in known_samples),
        'path_strength_max': max(s['path_strength'] for s in known_samples),
        'control_samples': len(samples)}
    payload = json.dumps(data, allow_nan=False)
    template = (REPO / 'tools/corner-replay-report.html').read_text()
    if template.count('__REPLAY_DATA__') != 1:
        raise ValueError('Template must have one data placeholder')
    # Prevent embedded fixture/notes text from terminating a script element.
    html = template.replace('__REPLAY_DATA__', payload.replace('<', '\\u003c'))
    out.mkdir(parents=True)
    (out / 'analysis.json').write_text(json.dumps(data, indent=2, allow_nan=False) + '\n')
    (out / 'report.html').write_text(html)
    print(json.dumps({'report': str(out / 'report.html'),
                      'measurements': data['measurements'], 'loopback': data.get('loopback')}))
    if args.loopback and not data['loopback']['matches_reference']:
        return 2


if __name__ == '__main__':
    sys.exit(main() or 0)

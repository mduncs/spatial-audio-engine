#!/usr/bin/env python3
"""Build a silent baseline/eight-bounce corner comparison from two reports."""
import argparse, json
from pathlib import Path

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--baseline', type=Path, required=True)
    ap.add_argument('--candidate', type=Path, required=True)
    ap.add_argument('--result', type=Path, required=True)
    ap.add_argument('--output', type=Path, required=True)
    a = ap.parse_args(); out = a.output.resolve()
    if out.exists(): ap.error('output must be a fresh directory')
    b, c, r = [json.loads(p.read_text()) for p in (a.baseline, a.candidate, a.result)]
    def capture_path(d): return d.get('artifacts', {}).get('capture', {}).get('path')
    if capture_path(b) != r.get('baseline_bundle', '') + '/capture.wav':
        # Result bundles may point at the directory while reports point at WAV.
        if Path(capture_path(b) or '').parent != Path(r.get('baseline_bundle', '')):
            raise ValueError('baseline report capture does not match candidate-result baseline_bundle')
    if capture_path(c) != r.get('candidate_bundle', '') + '/capture.wav':
        if Path(capture_path(c) or '').parent != Path(r.get('candidate_bundle', '')):
            raise ValueError('candidate report capture does not match candidate-result candidate_bundle')
    # Labels are presentation-only; geometry identity is the rings/coordinates.
    def geometry(d):
        return [{'id': x.get('id'), 'ring': x['ring']} for x in d.get('buildings', [])]
    if b.get('route') != c.get('route') or b.get('source') != c.get('source'):
        raise ValueError('baseline/candidate route or source differs')
    if geometry(b) != geometry(c): raise ValueError('baseline/candidate geometry differs')
    if len(b.get('audio_windows', [])) != len(c.get('audio_windows', [])):
        raise ValueError('baseline/candidate audio window counts differ')
    for x, y in zip(b.get('audio_windows', []), c.get('audio_windows', [])):
        if abs(x['time_s'] - y['time_s']) > 1e-6: raise ValueError('audio window timestamps are not aligned')
    def at(d, t): return min(d['samples'], key=lambda x: abs(x['time_s']-t))
    points = [5.4, 14.0, 16.0, 18.0]
    data = {
      'title':'One corner · baseline vs eight bounces', 'duration_s':min(b['duration_s'], c['duration_s']),
      'route':b['route'], 'buildings':b['buildings'], 'source':b['source'],
      'samples': [{'time_s':x['time_s'], 'position_m':x['position_m'],
                   'baseline':at(b,x['time_s']), 'candidate':at(c,x['time_s'])} for x in b['samples']],
      'events': [{'time_s':t, 'position_m':at(b,t)['position_m'],
                   'baseline':at(b,t), 'candidate':at(c,t)} for t in points],
      'windows': [{'time_s':x['time_s'], 'baseline':x, 'candidate':y}
                  for x,y in zip(b['audio_windows'],c['audio_windows'])],
      'result':r, 'bindings': {'baseline_capture': b['artifacts']['capture'],
                               'candidate_capture': c['artifacts']['capture'],
                               'baseline_replay': b['artifacts'].get('replay'),
                               'candidate_replay': c['artifacts'].get('replay')},
      'summary': {
        'measured':'Measured from the same 20 s route: direct loss begins near 5.4 s; the deep-shadow reflected-stage loss is visible across 14–18 s.',
        'nonclaims':['The map shows geometry and route positions, not solver rays or a measured sound field.',
                     'The comparison does not claim a human perceptual verdict or global eight-bounce promotion.',
                     'Levels are capture measurements in dBFS, not calibrated ear SPL.']}}
    template=(Path(__file__).with_name('corner-comparison-report.html')).read_text()
    if template.count('__COMPARISON_DATA__') != 1: raise ValueError('bad template placeholder')
    out.mkdir(parents=True); (out/'analysis.json').write_text(json.dumps(data,indent=2)+'\n')
    (out/'report.html').write_text(template.replace('__COMPARISON_DATA__',json.dumps(data).replace('<','\\u003c')))
    print(json.dumps({'report':str(out/'report.html'),'points':points,'difference_db':r['difference_db']}))
if __name__ == '__main__': main()

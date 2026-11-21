#!/usr/bin/env python3
"""Measure Docker startup-to-exit without image build or correctness validation."""
import argparse
import datetime
import json
import os
from pathlib import Path
import statistics
import subprocess
import tempfile
import time
import uuid

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--data', type=Path, required=True)
p.add_argument('--image', default='shopping-sessions:latest')
p.add_argument('--repeats', type=int, default=3)
p.add_argument('--timeout', type=float, default=3600)
p.add_argument('--seed', default='unknown', help='Label only; must match the dataset seed')
p.add_argument('--results', type=Path, default=Path('benchmarks/results'))
a = p.parse_args()
if not a.data.is_dir() or a.repeats < 1 or a.timeout <= 0:
    p.error('data must be an existing dataset directory; repeats and timeout must be positive')
a.results.mkdir(parents=True, exist_ok=True)
runs = []
for n in range(a.repeats):
    name = 'shopping-bench-' + uuid.uuid4().hex[:12]
    with tempfile.TemporaryDirectory(prefix='shopping-bench-') as root:
        out, scratch = Path(root)/'output', Path(root)/'scratch'
        out.mkdir(); scratch.mkdir(); os.chmod(root, 0o755); os.chmod(out, 0o777); os.chmod(scratch, 0o777)
        cmd = ['docker', 'run', '--name', name, '--platform', 'linux/amd64', '--network', 'none', '--read-only', '--tmpfs', '/tmp:rw,size=64m,mode=1777', '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges', '--pids-limit', '4096', '--cpus', '8', '--memory', '24g', '--memory-swap', '24g', '--user', '1000:1000', '-v', f'{a.data.resolve()}:/data:ro', '-v', f'{out}:/output', '-v', f'{scratch}:/scratch']
        for k, v in {'DATA_DIR':'/data', 'OUTPUT_DIR':'/output', 'SCRATCH_DIR':'/scratch', 'HOME':'/scratch', 'NCPU':'8', 'MEM_GB':'24'}.items(): cmd += ['-e', f'{k}={v}']
        cmd.append(a.image)
        start = time.monotonic()
        with tempfile.TemporaryFile(mode='w+') as log:
            proc = subprocess.Popen(cmd, stdout=log, stderr=subprocess.STDOUT)
            timed_out = False
            try: proc.wait(timeout=a.timeout)
            except subprocess.TimeoutExpired:
                timed_out = True
                subprocess.run(['docker', 'kill', name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                proc.wait()
            duration = time.monotonic() - start
            inspect = subprocess.run(['docker', 'inspect', name], capture_output=True, text=True)
            state = json.loads(inspect.stdout)[0]['State'] if inspect.returncode == 0 else {}
            subprocess.run(['docker', 'rm', '-f', name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            log.seek(0); output = log.read()[-4000:]
        record = {'run': n+1, 'seconds': duration, 'exit_code': proc.returncode, 'timeout': timed_out, 'oom_killed': state.get('OOMKilled'), 'peak_memory_bytes': None, 'log_tail': output}
        runs.append(record); print(json.dumps(record), flush=True)
report = {'dataset': str(a.data.resolve()), 'seed_label': a.seed, 'image': a.image, 'platform': 'linux/amd64', 'created_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(), 'median_seconds_successful': statistics.median([r['seconds'] for r in runs if r['exit_code'] == 0 and not r['timeout']]) if any(r['exit_code'] == 0 and not r['timeout'] for r in runs) else None, 'runs': runs}
path = a.results / (datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%S%fZ') + '.json')
path.write_text(json.dumps(report, indent=2) + '\n'); print(f'Results: {path}')
raise SystemExit(0 if all(r['exit_code'] == 0 and not r['timeout'] for r in runs) else 1)

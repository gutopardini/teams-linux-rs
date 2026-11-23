#!/usr/bin/env python3
"""Summarize numeric trace fields only. Never print arbitrary event payloads."""
import argparse
from collections import defaultdict
import json
import os
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('path', nargs='?', type=Path)
args = parser.parse_args()
root = Path(os.environ.get('XDG_STATE_HOME', Path.home() / '.local/state')) / 'teams-linux-rs/performance'
paths = sorted(root.glob('trace-*.jsonl'), key=lambda p: p.stat().st_mtime) if root.is_dir() else []
path = args.path or (paths[-1] if paths else None)
if path is None:
    raise SystemExit('No trace yet: reopen Teams using the installed tracing launcher.')
allowed = {'auto_presence_sent', 'auto_presence_failed', 'background_io', 'trace_started', 'widget_diff', 'layout', 'widget_event', 'cpu_draw', 'input_to_cpu_draw', 'view',
    'mouse_press', 'wheel', 'key_press', 'window_focus', 'update_select_chat', 'update_editor',
    'update_history_scroll', 'update_list_scroll', 'update_sync_tick', 'update_refresh',
    'update_initial_page', 'update_new_chat', 'update_avatar', 'update_other'}
groups = defaultdict(list)
worst = []
pending = {}
counts = {'max_messages': 0, 'max_chats': 0, 'dropped_samples': 0}
with path.open() as source:
    for line in source:
        try:
            row = json.loads(line)
            phase = row.get('phase')
            if phase not in allowed:
                continue
            numbers = [row.get(k, 0) for k in ['duration_us', 'at_us', 'messages', 'chats', 'dropped']]
            if not all(isinstance(n, int) and 0 <= n <= 2**64-1 for n in numbers):
                continue
            duration, at, messages, chats, dropped = numbers
            if row.get('edge') == 'start':
                pending[phase] = at / 1000
                continue
            pending.pop(phase, None)
            groups[phase].append(duration / 1000)
            worst.append({'phase': phase, 'duration_ms': duration / 1000, 'at_ms': at / 1000})
            counts['max_messages'] = max(counts['max_messages'], messages)
            counts['max_chats'] = max(counts['max_chats'], chats)
            counts['dropped_samples'] = max(counts['dropped_samples'], dropped)
        except (ValueError, AttributeError):
            continue  # A trailing line may still be being written.
summary = {}
for phase, samples in groups.items():
    samples.sort()
    summary[phase] = {'count': len(samples), 'p50_ms': samples[len(samples)//2],
        'p95_ms': samples[min(len(samples)-1, len(samples)*95//100)], 'max_ms': samples[-1]}
print(json.dumps({'path': str(path), 'counts': counts, 'phases': summary,
    'unfinished_phases_at_ms': pending,
    'slowest': sorted(worst, key=lambda r: r['duration_ms'], reverse=True)[:20],
    'limitations': 'Input receipt by Iced through CPU draw only; OS queue, GPU completion and monitor presentation excluded. First pending input per draw; overlapping inputs coalesced.'}, indent=2))

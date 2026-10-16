"""Diagnostic-only source control and pre-failure library context selection."""
import json
import sys
from pathlib import Path

if sys.argv[1] == 'patch':
    path = Path(sys.argv[2])
    source = path.read_text()
    replacements = [
        ('use std::cell::Cell;', 'use std::cell::Cell;\nuse std::sync::atomic::{AtomicUsize, Ordering};\n\nstatic ACTIVE_SCOPES: AtomicUsize = AtomicUsize::new(0);'),
        ('fn record(allocated: usize, released: usize) {', 'fn record(allocated: usize, released: usize) {\n    if ACTIVE_SCOPES.load(Ordering::Relaxed) == 0 { return; }'),
        ('MEASUREMENT.with(|slot| slot.set(Measurement::default()));', 'MEASUREMENT.with(|slot| slot.set(Measurement::default()));\n        ACTIVE_SCOPES.fetch_sub(1, Ordering::Relaxed);'),
        ('''    MEASUREMENT.with(|slot| {
        assert!(!slot.get().active, "allocation measurements cannot nest");
        slot.set(Measurement {''', '''    MEASUREMENT.with(|slot| assert!(!slot.get().active, "allocation measurements cannot nest"));
    ACTIVE_SCOPES.fetch_add(1, Ordering::Relaxed);
    let guard = MeasurementGuard;
    MEASUREMENT.with(|slot| {
        slot.set(Measurement {'''),
        ('    let guard = MeasurementGuard;\n    let result = operation();', '    let result = operation();'),
    ]
    for old, new in replacements:
        assert source.count(old) == 1, old
        source = source.replace(old, new)
    path.write_text(source)
else:
    root = Path(sys.argv[2])
    names = sorted(line.removesuffix(': test') for line in (root / 'catalog-head.txt').read_text(encoding='utf-8-sig').splitlines() if line.endswith(': test'))
    cutoff = 'download::orchestration::full::tests::full_sync_worker_budget_\uffff'
    allowed = {name for name in names if name <= cutoff or name == 'test_helpers::allocation::high_water_tracks_real_overlapping_allocations_and_reallocation'}
    tree = {}
    for name in names:
        node = tree
        for part in name.split('::'):
            node = node.setdefault(part, {})
    skips = []
    ambiguous = set()
    def walk(node, prefix):
        joined = '::'.join(prefix)
        descendants = [name for name in names if name == joined or name.startswith(joined + '::')]
        if prefix and not any(name in allowed for name in descendants):
            token = joined + ('::' if node else '')
            if not any(token in name for name in allowed):
                skips.append(token)
                return
            if not node:
                ambiguous.add(joined)
                return
        for part, child in node.items():
            walk(child, prefix + [part])
    walk(tree, [])
    actual = {name for name in names if not any(skip in name for skip in skips)}
    assert allowed <= actual
    assert actual - allowed <= ambiguous
    assert len([name for name in allowed if 'full_sync_worker_budget' in name]) == 4
    assert any('heif_sparse_metadata_heap' in name for name in allowed)
    assert any('heif_native_exif_near_' in name for name in allowed)
    args = [arg for skip in skips for arg in ['--skip', skip]]
    assert sum(len(arg) + 1 for arg in args) < 16000
    (root / 'context-args.json').write_text(json.dumps(args))
    (root / 'context-selected.txt').write_text('\n'.join(sorted(actual)) + '\n')
    (root / 'context-selection.json').write_text(json.dumps({'full_catalog': len(names), 'required': len(allowed), 'selected': len(actual), 'extra_ambiguous_names': sorted(actual - allowed), 'cutoff': cutoff, 'skip_tokens': len(skips)}, indent=2))

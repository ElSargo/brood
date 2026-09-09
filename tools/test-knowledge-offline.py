"""Prove embedded extension knowledge with the checkout hidden and no network."""
import argparse
import hashlib
import json
import pathlib
import subprocess
import tempfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--binary', type=pathlib.Path, required=True)
parser.add_argument('--out-dir', type=pathlib.Path, required=True)
args = parser.parse_args()
args.out_dir.mkdir(parents=True, exist_ok=True)
root = pathlib.Path(tempfile.mkdtemp(prefix='knowledge-', dir=args.out_dir)).resolve()
binary = args.binary.resolve()
base = ['bwrap', '--unshare-net', '--die-with-parent', '--ro-bind', '/nix/store', '/nix/store', '--ro-bind', str(binary), '/brood', '--bind', str(root), '/out', '--proc', '/proc', '--dev', '/dev', '--tmpfs', '/tmp', '--chdir', '/tmp', '--clearenv']
def run(*args):
    return subprocess.run(base + ['/brood', 'knowledge', *args], check=True, text=True, capture_output=True).stdout
metadata = json.loads(run('describe'))
assert metadata['error'] is None
assert metadata['endpoint'] == 'querying-binary'
search = json.loads(run('search', 'actor', '--limit', '2'))
assert len(search['data']['entries']) == 2
example = json.loads(run('get', 'examples/wasm-init-child/src/lib.rs'))
assert 'Guest' in example['data']['content']
run('export', '/out/knowledge.json')
bundle = json.loads((root / 'knowledge.json').read_text())
packaged_source = bundle.get('packaged_source')
if packaged_source is not None:
    assert packaged_source['modification_status'] in ('clean', 'modified', 'unavailable')
    if packaged_source['nar_hash'] is not None:
        assert packaged_source['nar_hash'].startswith('sha256-')
assert len(bundle['entries']) > 10
assert any(entry['id'] == 'CONTEXT.md' for entry in bundle['entries'])
assert any(entry['id'] == 'docs/progress/plugin-pure-renderer-final-report.json' for entry in bundle['entries'])
def common(query):
    return json.loads(subprocess.run(base + ['/brood', 'package', '--store', '/unavailable-store', 'query', json.dumps(query)], check=True, text=True, capture_output=True).stdout)
description = common({'version': 1, 'collection': 'bundled-description'})
assert description['error'] is None and description['endpoint'] == 'querying-binary'
assert description['data'][0]['content_digest'] == bundle['content_digest']
document = 'crates/brood-wasm/wit/brood-plugin.wit'
query = {'version': 1, 'collection': 'documents', 'document': document, 'limit': 2}
parts = []
while True:
    page = common(query)
    assert page['error'] is None
    parts.extend(chunk['content'] for chunk in page['data'])
    if page['next_offset'] is None:
        break
    query.update(offset=page['next_offset'], revision=page['revision'])
assert ''.join(parts) == next(entry['content'] for entry in bundle['entries'] if entry['id'] == document)
assert len(parts) > 2
search_common = common({'version': 1, 'collection': 'documents', 'document': 'brood.commands.v1', 'search': 'subcommands', 'limit': 1})
assert search_common['data'][0]['category'] == 'generated-contract'
generated = [entry for entry in bundle['entries'] if entry['category'] == 'generated-contract']
for symbol in ['brood.introspection.request.v1', 'brood.introspection.response.v1', 'brood.package-instance.v1', 'brood.package-service-event.v1', 'brood.runtime-config.v1', 'brood.realm-init-config.v1']:
    schema = json.loads(next(entry['content'] for entry in generated if entry['id'] == symbol))
    assert schema['$id'] == symbol
    assert '$schema' in schema
    assert isinstance(schema['x-rust-type'], str)
    assert any(entry['id'] == schema['x-source-document'] for entry in bundle['entries'])
schema_page = common({'version': 1, 'collection': 'documents', 'document': 'brood.introspection.request.v1', 'limit': 1})
assert schema_page['error'] is None and schema_page['data'][0]['category'] == 'generated-contract'

(root / 'proof.json').write_text(json.dumps({'checkout_visible':False, 'network_namespace':'isolated', 'entry_count':len(bundle['entries']), 'bundle_digest':bundle['content_digest'], 'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(), 'packaged_source': packaged_source, 'example':'examples/wasm-init-child/src/lib.rs', 'common_introspection':'description-search-exact-chunk-reconstruction-and-generated-schemas', 'outcome':'passed'}, indent=2))
print((root / 'proof.json').read_text())

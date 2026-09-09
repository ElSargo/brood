"""Real package lifecycle proof, invoked by Brood's no-host-window test CLI."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--binary', type=Path, required=True)
parser.add_argument('--client', type=Path, required=True)
parser.add_argument('--root', type=Path, required=True)
parser.add_argument('--out-dir', type=Path, required=True)
args = parser.parse_args()
if not os.environ.get('DBUS_SESSION_BUS_ADDRESS'):
    os.execvp('dbus-run-session', ['dbus-run-session', '--', 'python3', __file__, *os.sys.argv[1:]])
args.out_dir.mkdir(parents=True, exist_ok=True)
out = Path(tempfile.mkdtemp(prefix='packages-', dir=args.out_dir)).resolve()
runtime = Path(tempfile.mkdtemp(prefix='brood-pkg-'))
os.chmod(runtime, 0o700)
root = args.root.resolve()
binary = args.binary.resolve()
client_binary = args.client.resolve()
remote_binary = binary.with_name('brood-remote')
if not remote_binary.is_file():
    remote_path = shutil.which('brood-remote')
    if remote_path is None:
        raise RuntimeError('locate brood-remote beside Brood or on PATH')
    remote_binary = Path(remote_path).resolve()
store = out / 'store'
external = out / 'external-package'
source = root / 'examples/self-describing-pair'
shutil.copytree(source, external, ignore=shutil.ignore_patterns('target'))
shutil.copytree(root / 'crates/brood-package-contract', external / 'contract', ignore=shutil.ignore_patterns('target'))
(external / 'wit').mkdir(exist_ok=True)
shutil.copy2(root / 'crates/brood-wasm/wit/brood-plugin.wit', external / 'wit/brood-plugin.wit')
env = os.environ.copy()
env.pop('brood_session', None)
env['BROOD_PACKAGE_STORE'] = str(store)
env['XDG_RUNTIME_DIR'] = str(runtime)
env['RUST_LOG'] = 'broodd=info,brood_wasm=warn'
processes = []
files = []

def run(argv, **kwargs):
    return subprocess.run([str(arg) for arg in argv], check=True, text=True, capture_output=True, env={**env, "RUST_LOG": "warn"}, **kwargs).stdout

def start(argv, log, extra=None):
    handle = (out / log).open('w')
    files.append(handle)
    process = subprocess.Popen([str(arg) for arg in argv], env={**env, **(extra or {})}, stdout=handle, stderr=subprocess.STDOUT)
    processes.append(process)
    return process

def wait_for(probe, description, timeout=40):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            last = probe()
            if last:
                return last
        except (subprocess.CalledProcessError, json.JSONDecodeError, FileNotFoundError):
            pass
        time.sleep(0.1)
    raise AssertionError(f'timed out: {description}; last={last!r}; evidence={out}')

try:
    init = root / 'examples/package-init/target/wasm32-unknown-unknown/release/brood_package_init_example.component.wasm'
    run(['cargo', 'build', '--offline', '--locked', '--manifest-path', root / 'examples/package-init/Cargo.toml', '--target', 'wasm32-unknown-unknown', '--release'])
    run(['wasm-tools', 'component', 'new', init.with_name('brood_package_init_example.wasm'), '-o', init])
    run(['cargo', 'generate-lockfile', '--offline', '--manifest-path', external / 'Cargo.toml'])
    build_env = {
        'PATH': ':'.join(path for path in env['PATH'].split(':') if path.startswith('/nix/store/')),
        'CARGO_HOME': env.get('CARGO_HOME', str(Path.home() / '.cargo')),
        'CARGO_TARGET_DIR': str(out / 'build-cache'),
    }
    build_env.update({key: value for key, value in env.items() if key.startswith('NIX_')})
    recipe = '''set -eu
cargo build --offline --locked --release --target wasm32-unknown-unknown
wasm-tools component new "$CARGO_TARGET_DIR/wasm32-unknown-unknown/release/external_pair.wasm" -o session.wasm
wasm-tools component new "$CARGO_TARGET_DIR/wasm32-unknown-unknown/release/external_pair.wasm" -o presentation.wasm
'''
    (external / 'build.sh').write_text(recipe)
    (external / 'diagnostic.json').write_text(json.dumps({'type': 'object', 'required': ['ready'], 'properties': {'ready': {'type': 'boolean'}}, 'additionalProperties': False}))
    (external / 'peer.json').write_text(json.dumps({'type': 'object', 'required': ['version', 'message'], 'properties': {'version': {'const': 1}, 'message': {'enum': ['ping', 'pong']}}, 'additionalProperties': False}))
    manifest = '''schema-version = 1
[package]
id = "org.example.external-pair"
version = "1.0.0"
root = "session"
[[artifacts]]
id = "session"
kind = "wasm-component"
abi = "brood-component-v1"
source = "session.wasm"
placement = "session.wasm"
[[artifacts]]
id = "daemon"
kind = "wasm-component"
abi = "brood-component-v1"
source = "session.wasm"
placement = "daemon.wasm"
[[artifacts]]
id = "presentation"
kind = "wasm-component"
abi = "brood-component-v1"
source = "presentation.wasm"
placement = "presentation.wasm"
[extension]
schema-version = 1
purpose = "Real paired external actor acceptance"
source-revision = "inputs-are-content-addressed-in-the-plan"
state-compatibility = "stateless"
migration = "none"
[extension.host-interfaces]
"brood.package-activation" = [1]
"brood.package-services" = [1]
"brood.introspection" = [1]
"brood.package-diagnostics" = [1]
[extension.build]
inputs = ["Cargo.toml", "Cargo.lock", "src/lib.rs", "wit/brood-plugin.wit", "build.sh", "contract/Cargo.toml", "contract/src/lib.rs", "contract/src/services.rs", "contract/src/introspection.rs"]
executable = EXECUTABLE
arguments = ["build.sh", "contract/Cargo.toml", "contract/src/lib.rs", "contract/src/services.rs", "contract/src/introspection.rs"]
[extension.build.environment]
ENVIRONMENT
[[extension.documents]]
id = "external-pair.overview"
path = "README.md"
category = "example"
references = ["manifest"]
schemas = ["schema:interface:example.peer", "schema:diagnostic:org.example.external-pair.health"]
[[extension.diagnostics]]
id = "org.example.external-pair.health"
version = 1
schema = "diagnostic.json"
[[extension.interfaces]]
id = "example.peer"
version = 1
schema = "peer.json"
multiplicity = "one"
disconnect = "stop"
[[extension.activation]]
artifact = "daemon"
realm = "daemon"
scope = "per-realm"
resource-expectations = { timers = 1, memory-bytes = 1048576 }
restart = "never"
deactivation = "stop"
[[extension.activation]]
artifact = "session"
realm = "surface-session"
scope = "per-realm"
resource-expectations = { timers = 1, memory-bytes = 1048576 }
provides = ["example.peer"]
restart = "never"
deactivation = "stop"
[[extension.activation]]
artifact = "presentation"
realm = "presentation-client"
scope = "per-realm"
resource-expectations = { timers = 1, memory-bytes = 1048576 }
requires = ["example.peer"]
restart = "never"
deactivation = "stop"
'''.replace('EXECUTABLE', json.dumps(str(Path(shutil.which('bash')).resolve()))).replace('ENVIRONMENT', '\n'.join(f'{json.dumps(key)} = {json.dumps(value)}' for key, value in build_env.items()))
    manifest_path = external / 'package.toml'
    manifest_path.write_text(manifest)
    plan = out / 'plan.json'
    planned = json.loads(run([binary, 'package', '--store', store, 'plan', manifest_path, '--output', plan]))
    assert {activation['realm'] for activation in planned['manifest']['extension']['activation']} == {'daemon', 'surface-session', 'presentation-client'}
    assert all('capabilities' in artifact for artifact in planned['manifest']['artifacts'])
    assert planned['previous_selection'] is None and planned['executable_digest']
    assert planned['activation'] == 'pending realm init acknowledgement'
    (out / 'plan-description.json').write_text(json.dumps(planned, indent=2))
    # Applying must use the snapshot, not a hidden checkout dependency.
    shutil.rmtree(external)
    init = root / 'examples/package-init/target/wasm32-unknown-unknown/release/brood_package_init_example.component.wasm'
    config = out / 'brood.toml'
    config.write_text(f'[init]\nbackend = {json.dumps(str(init))}\nsession = {json.dumps(str(init))}\nclient = {json.dumps(str(init))}\n')
    destination = f'org.brood.PackageTest.p{os.getpid()}'
    server = start([binary, 'run', '--no-session-runner', '--headless', '--config', config, '--dbus-destination', destination], 'server.log')
    def ctl(*argv):
        return run([binary, 'ctl', '--dbus-destination', destination, *argv])
    wait_for(lambda: json.loads(ctl('surface-session', 'list-json')), 'existing session')
    before = json.loads(ctl('package-instances'))
    assert not before['instances']
    sway_config = out / 'sway.conf'
    sway_config.write_text('xwayland disable\noutput * mode 800x600\nseat seat0 fallback true\ndefault_border none\n')
    sway = start(['sway', '--unsupported-gpu', '--config', sway_config], 'compositor.log', {
        'WLR_BACKENDS': 'headless', 'WLR_HEADLESS_OUTPUTS': '1', 'WLR_LIBINPUT_NO_DEVICES': '1'})
    socket = wait_for(lambda: next((path for path in runtime.glob('wayland-*') if path.is_socket()), None), 'offscreen compositor socket')
    sway_socket = wait_for(lambda: next(runtime.glob('sway-ipc.*.sock'), None), 'compositor control socket')
    client = start([client_binary, '--config', config, '--dbus-destination', destination], 'client.log', {'WAYLAND_DISPLAY': socket.name})
    wait_for(lambda: 'configured presentation surface' in (out / 'client.log').read_text(), 'existing presentation realm')
    installed = json.loads(run([binary, 'package', '--store', store, 'apply', plan]))
    def session_records(count):
        snapshot = json.loads(ctl('package-instances'))
        live = [record for record in snapshot['instances'] if record['lifecycle'] == 'running' and record['realm'] == 'surface-session']
        daemon = [record for record in snapshot['instances'] if record['realm'] == 'daemon' and record['lifecycle'] == 'running']
        return snapshot if len(live) == count and len(daemon) == 1 else None
    first = wait_for(lambda: session_records(1), 'existing realm package activation', 60)
    description_query = {'version': 1, 'collection': 'package-description', 'package': 'org.example.external-pair', 'installation': installed['plan_digest']}
    common_description = json.loads(ctl('package-query', json.dumps(description_query)))
    assert common_description['error'] is None
    assert common_description['data'][0]['identity']['knowledge_digest'] == installed['knowledge_digest']

    pagination_changes = []
    def stable_common_pages():
        common_request = {'version': 1, 'collection': 'instances', 'limit': 1}
        first_page = json.loads(ctl('package-query', json.dumps(common_request)))
        assert first_page['schema'] == 'brood.introspection.response.v1' and first_page['error'] is None
        assert first_page['total'] == 2 and first_page['next_offset'] == 1
        common_request.update(offset=1, revision=first_page['revision'])
        next_page = json.loads(ctl('package-query', json.dumps(common_request)))
        if next_page['error'] and next_page['error']['code'] == 'revision-changed':
            pagination_changes.append({'first_revision': first_page['revision'], 'next_revision': next_page['revision']})
            (out / 'pagination-restarts.json').write_text(json.dumps(pagination_changes, indent=2))
            return None
        assert next_page['error'] is None and next_page['next_offset'] is None, next_page
        assert next_page['data'][0]['actor'] != first_page['data'][0]['actor']
        return first_page, next_page
    common_first, common_next = wait_for(stable_common_pages, 'stable revision across common instance pages')
    invalid_query = json.loads(ctl('package-query', '{}'))
    assert invalid_query['error']['code'] == 'invalid-request'
    offline = json.loads(run([binary, 'package', '--store', store, 'query', json.dumps({'version': 1, 'collection': 'installations'})]))
    assert offline['schema'] == common_first['schema'] and offline['error'] is None
    assert offline['data'][0]['installation']['plan_digest'] == installed['plan_digest']
    (out / 'common-introspection.json').write_text(json.dumps({'first': common_first, 'second': common_next, 'invalid': invalid_query, 'offline': offline}, indent=2))
    (out / 'session-existing.json').write_text(json.dumps(first, indent=2))
    def ready_binding():
        snapshot = json.loads(ctl('package-instances'))
        service = snapshot['services']
        return snapshot if any(binding['status'] == 'ready' for binding in service['bindings']) and service['validated_messages'] >= 2 else None
    bound = wait_for(ready_binding, 'acknowledged typed peer binding and schema-validated ping/pong', 60)
    def failed_diagnostics():
        current = json.loads(ctl('package-instances'))
        instances = [record for record in current['instances'] if record['realm'] == 'surface-session']
        instances += [entry['facts'] for entry in current['services']['instances'] if entry['availability'] == 'connected']
        return current if len(instances) == 2 and all(record['lifecycle'] == 'running' and record.get('diagnostics', {}).get('org.example.external-pair.health', {}).get('validation') == 'rejected' for record in instances) else None
    diagnostic_snapshot = wait_for(failed_diagnostics, 'invalid plugin diagnostics fail independently of running paired actors')
    (out / 'diagnostic-failure.json').write_text(json.dumps(diagnostic_snapshot, indent=2))
    (out / 'peer-binding.json').write_text(json.dumps(bound, indent=2))
    repeated = json.loads(run([binary, 'package', '--store', store, 'apply', plan]))
    assert installed['plan_digest'] == repeated['plan_digest']
    identities = lambda snapshot: [(record['actor'], record['generation'], record['declaration']['identity']) for record in snapshot['instances']]
    assert identities(first) == identities(json.loads(ctl('package-instances')))
    assert any(record['realm'] == 'daemon' and record['lifecycle'] == 'running' for record in first['instances'])
    disabled = json.loads(run([binary, 'package', '--store', store, 'disable', 'org.example.external-pair']))
    assert disabled['selection'] == 'disabled' and disabled['retained']['artifacts']
    def stopped_instances():
        snapshot = json.loads(ctl('package-instances'))
        remote = [record['facts'] for record in snapshot['services']['instances'] if record['availability'] == 'connected']
        return snapshot if snapshot['instances'] and remote and all(record['lifecycle'] == 'stopped' for record in [*snapshot['instances'], *remote]) else None
    stopped = wait_for(stopped_instances, 'disable tears down all owned generations')
    assert all(not record['resources'] for record in stopped['instances'])
    (out / 'disabled.json').write_text(json.dumps(stopped, indent=2))
    repeated_disable = json.loads(run([binary, 'package', '--store', store, 'disable', 'org.example.external-pair']))
    assert not repeated_disable['changed']
    run([binary, 'package', '--store', store, 'enable', 'org.example.external-pair'])
    enabled = wait_for(ready_binding, 'enable recreates typed peers')
    (out / 'enabled.json').write_text(json.dumps(enabled, indent=2))
    ctl('surface-session', 'new')
    later = wait_for(lambda: session_records(2), 'subsequent realm package activation')
    (out / 'session-later.json').write_text(json.dumps(later, indent=2))
    assert {record['declaration']['identity']['installation'] for record in later['instances']} == {installed['plan_digest']}
    wait_for(lambda: 'started presentation WASM actor id=package:org.example.external-pair:presentation' in (out / 'client.log').read_text(), 'presentation package activation', 60)
    wait_for(ready_binding, 'binding recovered after session topology change')
    ctl('close-frontend')
    client.wait(timeout=20)
    snapshots = [json.loads(line.removeprefix('PACKAGE_INSTANCES ')) for line in (out / 'client.log').read_text().splitlines() if line.startswith('PACKAGE_INSTANCES ')]
    assert snapshots and len(snapshots[-1]['instances']) == 1
    presentation = snapshots[-1]['instances'][0]
    assert presentation['lifecycle'] in ('running', 'stopping', 'stopped')
    assert presentation['declaration']['identity']['installation'] == installed['plan_digest']
    (out / 'presentation.json').write_text(json.dumps(snapshots[-1], indent=2))
    disconnected = wait_for(lambda: (snapshot if not snapshot['services']['bindings'] and all(endpoint['status'] == 'unreachable' for endpoint in snapshot['services']['endpoints']) else None) if (snapshot := json.loads(ctl('package-instances'))) else None, 'disconnected endpoint observation')
    (out / 'disconnected.json').write_text(json.dumps(disconnected, indent=2))
    second_client = start([client_binary, '--config', config, '--dbus-destination', destination], 'client-later.log', {'WAYLAND_DISPLAY': socket.name})
    wait_for(lambda: 'started presentation WASM actor id=package:org.example.external-pair:presentation' in (out / 'client-later.log').read_text(), 'subsequent presentation activation', 60)
    wait_for(ready_binding, 'subsequent presentation peer binding')
    replacement_sequence = iter(range(100))
    def replacement(feature, version, compatibility="stateless", migration="none", host_version=1):
        captured = json.loads(plan.read_text())
        external.mkdir(exist_ok=True)
        for path, content in captured['inputs'].items():
            target = external / path
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(bytes(content))
        updated_recipe = recipe.replace('wasm-tools component new "$CARGO_TARGET_DIR/wasm32-unknown-unknown/release/external_pair.wasm" -o presentation.wasm', f'cargo build --offline --locked --release --target wasm32-unknown-unknown --features {feature}\nwasm-tools component new "$CARGO_TARGET_DIR/wasm32-unknown-unknown/release/external_pair.wasm" -o presentation.wasm')
        (external / 'build.sh').write_text(updated_recipe)
        manifest_path.write_text(manifest.replace('"brood.introspection" = [1]', f'"brood.introspection" = [{host_version}]').replace('version = "1.0.0"', f'version = "{version}"').replace('state-compatibility = "stateless"', f'state-compatibility = "{compatibility}"').replace('migration = "none"', f'migration = "{migration}"'))
        replacement_plan = out / f'{feature}-{next(replacement_sequence)}-plan.json'
        run([binary, 'package', '--store', store, 'plan', manifest_path, '--output', replacement_plan])
        shutil.rmtree(external)
        return json.loads(run([binary, 'package', '--store', store, 'apply', replacement_plan]))
    before_failed_build = json.loads(ctl('package-instances'))
    try:
        replacement('missing-feature-build-proof', '0.9.8')
    except subprocess.CalledProcessError as error:
        (out / 'failed-build-command.txt').write_text(error.stderr)
        assert 'installation attempt preserved at' in error.stderr
    else:
        raise AssertionError('invalid build unexpectedly applied')
    still_selected = json.loads(run([binary, 'package', '--store', store, 'inspect', 'org.example.external-pair']))
    assert still_selected['plan_digest'] == installed['plan_digest']
    assert identities(before_failed_build) == identities(json.loads(ctl('package-instances')))
    failed_plan = json.loads((out / 'missing-feature-build-proof-0-plan.json').read_text())
    failed_attempts = list((store / 'attempts').glob(f"{failed_plan['digest']}-*/failure.json"))
    assert len(failed_attempts) == 1
    failed_build = json.loads(failed_attempts[0].read_text())
    assert failed_build['phase'] == 'build' and failed_build['activation'] == 'not-attempted'
    (out / 'failed-build.json').write_text(json.dumps(failed_build, indent=2))
    unsupported_host = replacement('incompatible', '0.9.9', host_version=99)
    host_pending = wait_for(stopped_instances, 'unsupported host contract retires every package instance', 60)
    assert any('host interface brood.introspection' in (actor.get('reason') or '') for policy in host_pending['catalogue']['root_policy'] for actor in policy.get('report', {}).get('actors', []))
    (out / 'unsupported-host.json').write_text(json.dumps(host_pending, indent=2))
    incompatible = replacement('incompatible', '1.0.1')
    def incompatible_observation():
        snapshot = json.loads(ctl('package-instances'))
        peers = [peer for peer in snapshot['services']['peers'] if peer['peer']['realm'] == 'presentation-client' and peer['peer']['identity']['installation'] == incompatible['plan_digest']]
        return snapshot if peers and peers[0]['peer']['registered'].get('example.peer') == [2] and peers[0]['readiness'] == 'pending' and not snapshot['services']['bindings'] else None
    pending = wait_for(incompatible_observation, 'incompatible replacement remains pending', 60)
    (out / 'incompatible-peer.json').write_text(json.dumps(pending, indent=2))
    failed = replacement('fail-start', '1.0.2')
    def failed_observation():
        snapshot = json.loads(ctl('package-instances'))
        instances = [instance['facts'] for instance in snapshot['services']['instances'] if instance['availability'] == 'connected']
        return snapshot if any(instance['declaration']['identity']['installation'] == failed['plan_digest'] and instance['lifecycle'] == 'failed' for instance in instances) else None
    broken = wait_for(failed_observation, 'failed replacement is observable through host control', 60)
    (out / 'failed-replacement.json').write_text(json.dumps(broken, indent=2))
    ctl('close-frontend')
    second_client.wait(timeout=20)
    later_clients = [json.loads(line.removeprefix('PACKAGE_INSTANCES ')) for line in (out / 'client-later.log').read_text().splitlines() if line.startswith('PACKAGE_INSTANCES ')]
    assert later_clients and len(later_clients[-1]['instances']) == 1
    assert later_clients[-1]['instances'][0]['declaration']['identity']['installation'] == failed['plan_digest']
    assert later_clients[-1]['instances'][0]['lifecycle'] == 'failed'
    (out / 'presentation-later.json').write_text(json.dumps(later_clients[-1], indent=2))
    migration_client = start([client_binary, '--config', config, '--dbus-destination', destination], 'migration-client.log', {'WAYLAND_DISPLAY': socket.name})
    state_one = replacement('incompatible', '2.0.0', 'schema-v1', 'manual')
    def all_running_revision(revision):
        snapshot = json.loads(ctl('package-instances'))
        remote = [entry['facts'] for entry in snapshot['services']['instances'] if entry['availability'] == 'connected']
        records = [*snapshot['instances'], *remote]
        return snapshot if len(records) == 4 and all(record['lifecycle'] == 'running' and record['declaration']['identity']['installation'] == revision for record in records) else None
    wait_for(lambda: all_running_revision(state_one['plan_digest']), 'first state format activated', 60)
    mutable_state = out / 'operator-managed-state.json'
    mutable_state.write_text('{"schema":"schema-v1","retained":"user data"}')
    state_two = replacement('incompatible', '2.0.1', 'schema-v2', 'manual')
    waiting = wait_for(stopped_instances, 'incompatible state stops all old generations before migration', 60)
    status = json.loads(run([binary, 'package', '--store', store, 'migration-status', 'org.example.external-pair']))
    assert status['status'] == 'migration-required' and status['state']['compatibility'] == 'schema-v1'
    assert json.loads(mutable_state.read_text())['schema'] == 'schema-v1'
    assert any(actor.get('reason') and 'state format' in actor['reason'] for root_policy in waiting['catalogue']['root_policy'] for actor in root_policy.get('report', {}).get('actors', []))
    (out / 'migration-pending.json').write_text(json.dumps(waiting, indent=2))
    # The external/manual migration owns data changes. The host only records
    # the operator's exact-revision completion attestation after quiescence.
    mutable_state.write_text('{"schema":"schema-v2","retained":"user data"}')
    run([binary, 'package', '--store', store, 'migration-complete', 'org.example.external-pair', '--installation', state_two['plan_digest'], '--previous-state', 'schema-v1', '--evidence', 'test operator migrated schema after verifying all four generations stopped'])
    resumed = wait_for(lambda: all_running_revision(state_two['plan_digest']), 'migration completion releases new generations', 60)
    (out / 'migration-complete.json').write_text(json.dumps(resumed, indent=2))
    rollback = replacement('incompatible', '2.0.0', 'schema-v1', 'manual')
    wait_for(stopped_instances, 'code rollback cannot silently roll back mutable state', 60)
    rollback_status = json.loads(run([binary, 'package', '--store', store, 'migration-status', 'org.example.external-pair']))
    assert rollback_status['status'] == 'migration-required' and rollback_status['state']['compatibility'] == 'schema-v2'
    assert json.loads(mutable_state.read_text())['schema'] == 'schema-v2'
    # Acquire a complete compatible prebuilt set with no build inputs or toolchain.
    external.mkdir()
    captured = json.loads(plan.read_text())
    for name in ['README.md', 'peer.json', 'diagnostic.json']:
        (external / name).write_bytes(bytes(captured['inputs'][name]))
    prebuilt_manifest = manifest.replace('version = "1.0.0"', 'version = "3.0.0"').replace('state-compatibility = "stateless"', 'state-compatibility = "schema-v2"')
    prebuilt_manifest = '\n'.join('executable = "/nix/store/unavailable-toolchain/bin/bash"' if line.startswith('executable =') else line for line in prebuilt_manifest.splitlines())
    prebuilt_manifest += '\n[[extension.prebuilt]]\nid = "captured-release"\nhost-interfaces = { "brood.introspection" = [1] }\n[extension.prebuilt.artifacts]\n'
    for artifact in ['session', 'daemon', 'presentation']:
        content = (store / 'objects' / installed['plan_digest'] / f'{artifact}.wasm').read_bytes()
        (external / f'{artifact}.wasm').write_bytes(content)
        prebuilt_manifest += f'{artifact} = {{ path = "{artifact}.wasm", sha256 = "{hashlib.sha256(content).hexdigest()}" }}\n'
    manifest_path.write_text(prebuilt_manifest)
    prebuilt_plan = out / 'prebuilt-plan.json'
    run([binary, 'package', '--store', store, 'plan', manifest_path, '--output', prebuilt_plan])
    assert json.loads(prebuilt_plan.read_text())['prebuilt'] == 'captured-release'
    shutil.rmtree(external)
    acquired = json.loads(run([binary, 'package', '--store', store, 'apply', prebuilt_plan]))
    acquired_live = wait_for(lambda: all_running_revision(acquired['plan_digest']), 'prebuilt artifacts activate without source or toolchain', 60)
    wait_for(ready_binding, 'prebuilt paired actors bind')
    (out / 'prebuilt-instances.json').write_text(json.dumps(acquired_live, indent=2))
    # An installed library has no actors of its own. Its exact selected revision
    # must nevertheless gate the paired package's real, already-running actors.
    external.mkdir()
    for path, content in json.loads(prebuilt_plan.read_text())['inputs'].items():
        target = external / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(bytes(content))
    dependency_source = external / 'vendor'
    dependency_source.mkdir()
    for name in ['README.md', 'peer.json', 'diagnostic.json', 'session.wasm', 'daemon.wasm', 'presentation.wasm']:
        shutil.copy2(external / name, dependency_source / name)
    dependency_manifest = prebuilt_manifest.split('[[extension.activation]]', 1)[0]
    dependency_manifest += '[[extension.prebuilt]]' + prebuilt_manifest.split('[[extension.prebuilt]]', 1)[1]
    dependency_manifest = dependency_manifest.replace('[extension]\n', '[extension]\nactivation = []\n').replace('org.example.external-pair', 'org.example.library')
    (dependency_source / 'package.toml').write_text(dependency_manifest)
    manifest_path.write_text(prebuilt_manifest + '\n[[extension.dependencies]]\npackage = "org.example.library"\nmanifest = "vendor/package.toml"\n')
    dependency_plan = out / 'dependency-plan.json'
    run([binary, 'package', '--store', store, 'plan', manifest_path, '--output', dependency_plan])
    shutil.rmtree(external)
    dependent = json.loads(run([binary, 'package', '--store', store, 'apply', dependency_plan]))
    dependency_live = wait_for(lambda: all_running_revision(dependent['plan_digest']), 'captured dependency graph activates four dependent instances', 60)
    wait_for(ready_binding, 'dependent paired actors bind')
    assert dependent['dependencies']['org.example.library']
    dependency_disabled = json.loads(run([binary, 'package', '--store', store, 'disable', 'org.example.library']))
    assert any(entry.get('package') == 'org.example.external-pair' for entry in dependency_disabled['potential_dependents'])
    dependency_stopped = wait_for(stopped_instances, 'disabled installation dependency retires four dependent instances', 60)
    assert any('dependency' in (actor.get('reason') or '') for policy in dependency_stopped['catalogue']['root_policy'] for actor in policy.get('report', {}).get('actors', []))
    run([binary, 'package', '--store', store, 'enable', 'org.example.library'])
    dependency_resumed = wait_for(lambda: all_running_revision(dependent['plan_digest']), 'restored exact dependency starts fresh dependent generations', 60)
    assert identities(dependency_live) != identities(dependency_resumed)
    wait_for(ready_binding, 'restored dependent peers bind')
    (out / 'dependency-disabled.json').write_text(json.dumps(dependency_stopped, indent=2))
    (out / 'dependency-resumed.json').write_text(json.dumps(dependency_resumed, indent=2))
    ctl('close-frontend')
    migration_client.wait(timeout=20)
    removed = json.loads(run([binary, 'package', '--store', store, 'remove', 'org.example.external-pair']))
    assert removed['selection'] == 'removed' and removed['retained']['mutable_state']
    repeated_remove = json.loads(run([binary, 'package', '--store', store, 'remove', 'org.example.external-pair']))
    assert not repeated_remove['changed']
    wait_for(lambda: all(record['lifecycle'] == 'stopped' for record in json.loads(ctl('package-instances'))['instances']), 'remove retires daemon and session actors')
    history = json.loads(run([binary, 'package', '--store', store, 'knowledge', 'org.example.external-pair', '--installation', installed['plan_digest']]))
    assert history['plan_digest'] == installed['plan_digest']
    historical_query = {'version': 1, 'collection': 'package-documents', 'package': 'org.example.external-pair', 'installation': installed['plan_digest'], 'document': 'external-pair.overview'}
    historical_documents = json.loads(run([binary, 'package', '--store', store, 'query', json.dumps(historical_query)]))
    assert historical_documents['error'] is None
    assert historical_documents['data'][0]['identity']['knowledge_digest'] == installed['knowledge_digest']
    assert historical_documents['data'][0]['content'] == history['documents'][0]['content']
    (out / 'historical-common-knowledge.json').write_text(json.dumps(historical_documents, indent=2))
    historical = json.loads(run([binary, 'package', '--store', store, 'inspect', 'org.example.external-pair', '--installation', installed['plan_digest']]))
    assert historical['selection_status'] == 'historical' 
    # Reuse the authenticated artifact lane with three disjoint stores. Each
    # destination installs only its explicitly authorized realm from the bundle.
    bundle = out / 'external-pair.brood-package'
    run([binary, 'package', '--store', store, 'export', 'org.example.external-pair', '--installation', dependent['plan_digest'], '--output', bundle])
    library_bundle = out / 'library.brood-package'
    run([binary, 'package', '--store', store, 'export', 'org.example.library', '--installation', dependent['dependencies']['org.example.library'], '--output', library_bundle])
    server_cert, server_key = out / 'server.der', out / 'server-key.der'
    client_cert, client_key = out / 'client.der', out / 'client-key.der'
    for cert, key in [(server_cert, server_key), (client_cert, client_key)]:
        run([remote_binary, 'generate-certificate', '--certificate', cert, '--private-key', key])
    def remote_round(authorized, allow_library=True):
        name = ('authorized' if authorized else 'denied') if allow_library else 'denied-library'
        remote_destination = f'org.brood.PackageRemote.p{os.getpid()}{name}'
        remote_store = out / f'{name}-server-store'
        client_store = out / f'{name}-client-store'
        destination_server = start([binary, 'run', '--no-session-runner', '--headless', '--config', config, '--dbus-destination', remote_destination], f'{name}-server.log', {'BROOD_PACKAGE_STORE': str(remote_store)})
        def remote_ctl(*argv):
            return run([binary, 'ctl', '--dbus-destination', remote_destination, *argv])
        wait_for(lambda: json.loads(remote_ctl('surface-session', 'list-json')), 'remote destination session')
        relay_args = [remote_binary, 'relay', '--certificate', server_cert, '--private-key', server_key, '--trusted-client-certificate', client_cert, '--dbus-destination', remote_destination, '--offer-client-artifact', f'org.example.external-pair={bundle}', '--artifact-cache', out / f'{name}-relay-cache']
        relay_args.extend(['--offer-client-artifact', f'org.example.library={library_bundle}'])
        if allow_library:
            relay_args.extend(['--allow-session-artifact', 'org.example.library'])
        if authorized:
            relay_args.extend(['--allow-session-artifact', 'org.example.external-pair'])
        relay = start(relay_args, f'{name}-relay.log')
        address = wait_for(lambda: next((line.split()[0].removeprefix('LISTEN=') for line in (out / f'{name}-relay.log').read_text().splitlines() if line.startswith('LISTEN=')), None), 'authenticated relay listener')
        presentation = start([client_binary, '--config', config, '--remote-connect', address, '--remote-server-certificate', server_cert, '--remote-client-certificate', client_cert, '--remote-client-private-key', client_key, '--remote-offer-session-artifact', f'org.example.external-pair={bundle}', '--remote-offer-session-artifact', f'org.example.library={library_bundle}', '--remote-allow-client-artifact', 'org.example.external-pair', '--remote-allow-client-artifact', 'org.example.library'], f'{name}-client.log', {'WAYLAND_DISPLAY': socket.name, 'BROOD_PACKAGE_STORE': str(client_store), 'XDG_CACHE_HOME': str(out / f'{name}-client-cache')})
        def observed():
            snapshot = json.loads(remote_ctl('package-instances'))
            remote_instances = [entry['facts'] for entry in snapshot['services']['instances'] if entry['availability'] == 'connected']
            if not remote_instances or not all(record['realm'] == 'presentation-client' and record['lifecycle'] == 'running' for record in remote_instances):
                return None
            if authorized and allow_library:
                return snapshot if len(snapshot['instances']) == 1 and snapshot['instances'][0]['realm'] == 'surface-session' and any(binding['status'] == 'ready' for binding in snapshot['services']['bindings']) and snapshot['services']['validated_messages'] >= 2 else None
            rejected = 'not authorized' in (out / f'{name}-client.log').read_text()
            if authorized:
                pending = any('dependency org.example.library' in (actor.get('reason') or '') for policy in snapshot['catalogue']['root_policy'] for actor in policy.get('report', {}).get('actors', []))
                return snapshot if rejected and pending and not snapshot['instances'] and not snapshot['services']['bindings'] else None
            return snapshot if rejected and not snapshot['instances'] and not snapshot['services']['bindings'] else None
        snapshot = wait_for(observed, f'{name} remote package distribution and destination outcomes', 60)
        (out / f'{name}-distribution.json').write_text(json.dumps(snapshot, indent=2))
        assert (client_store / 'selected/org.example.external-pair').exists()
        assert (remote_store / 'selected/org.example.external-pair').exists() == authorized
        assert (client_store / 'selected/org.example.library').read_text().strip() == dependent['dependencies']['org.example.library']
        assert (remote_store / 'selected/org.example.library').exists() == allow_library
        if allow_library:
            assert (remote_store / 'selected/org.example.library').read_text().strip() == dependent['dependencies']['org.example.library']
        if authorized and allow_library:
            run([binary, 'package', '--store', remote_store, 'disable', 'org.example.external-pair'])
            def consumer_stopped():
                current = json.loads(remote_ctl('package-instances'))
                peers = [entry['facts'] for entry in current['services']['instances'] if entry['availability'] == 'connected']
                return current if peers and all(record['lifecycle'] == 'stopped' for record in peers) and not current['services']['bindings'] else None
            lost = wait_for(consumer_stopped, 'root stops remote consumer after required provider disappears', 60)
            (out / 'disconnect-stop.json').write_text(json.dumps(lost, indent=2))
            run([binary, 'package', '--store', remote_store, 'enable', 'org.example.external-pair'])
            resumed = wait_for(observed, 'root restarts consumer after compatible provider returns', 60)
            (out / 'disconnect-resumed.json').write_text(json.dumps(resumed, indent=2))
        presentation.terminate()
        presentation.wait(timeout=15)
        relay.terminate()
        relay.wait(timeout=15)
        destination_server.terminate()
        destination_server.wait(timeout=15)
    remote_round(True)
    remote_round(False)
    remote_round(True, allow_library=False)
    report = {'outcome': 'passed', 'source_removed_before_apply': True, 'prebuilt': 'captured-artifacts-activate-without-source-or-toolchain', 'existing_session_instances': 1,
              'subsequent_session_instances': 2, 'presentation_instances': 1, 'subsequent_presentation_instances': 1, 'repeat_duplicated': False, 'common_introspection': 'offline-and-connected-pagination', 'guest_introspection': 'exact-installed-and-executable-documents-before-registration', 'guest_resource_sampling': 'allocated-timer-observed-before-registration', 'diagnostics': 'schema-rejection-independent-of-running-actor', 'host_contracts': 'unsupported-version-pending-before-start', 'resource_expectations': 'observed-timer-and-explicit-unsupported-memory-sampling',
              'state_migration': 'quiescence-explicit-attestation-and-rollback-guard', 'root_policy': 'acknowledged-with-pending-reasons', 'distribution': 'authenticated-artifact-lane-with-disjoint-stores', 'denied_destination': 'explicit-partial-success', 'daemon_instances': 1, 'disable_enable': 'teardown-and-reactivation', 'remove': 'state-and-artifacts-retained', 'disconnect_stop': 'root-retirement-and-rebind', 'incompatible_peer': 'pending', 'disconnected_endpoint': 'unreachable', 'failed_replacement': 'failed',
              'plan_digest': installed['plan_digest'], 'peer_binding': 'acknowledged-with-schema-validated-ping-pong', 'evidence': str(out)}
    report['executables'] = {
        name: {'path': str(path), 'sha256': hashlib.sha256(path.read_bytes()).hexdigest()}
        for name, path in [('brood', binary), ('client', client_binary), ('remote', remote_binary)]
    }
    report['failed_build'] = 'public-apply-preserves-selection-and-generations-with-phase-evidence'
    report['installation_dependency'] = 'captured-library-disable-retires-four-dependent-instances-and-enable-restarts'
    report['remote_dependencies'] = 'exact-library-transfer-both-directions-with-independent-destination-authorization-and-explicit-pending-on-denial'
    (out / 'report.json').write_text(json.dumps(report, indent=2))
    print(json.dumps(report, indent=2))
finally:
    for process in reversed(processes):
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
    for handle in files:
        handle.close()
    shutil.rmtree(runtime)

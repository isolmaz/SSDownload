"""Generate resolved dependency SBOM and copy original license/notice texts.

No downloaded programs are executed. Cargo metadata describes the Windows target
graph, including build dependencies, rather than claiming binary reachability.
"""
import argparse
import hashlib
import json
import re
import shutil
import subprocess
import tomllib
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument('--output', type=Path, required=True)
parser.add_argument('--nsis', type=Path, required=True)
parser.add_argument('--toolchain', default='')
parser.add_argument('--offline', action='store_true')
args = parser.parse_args()
root = Path(__file__).resolve().parent.parent
command = ['cargo'] + ([f'+{args.toolchain}'] if args.toolchain else [])
command += ['metadata', '--locked', '--format-version', '1', '--filter-platform', 'x86_64-pc-windows-msvc']
if args.offline:
    command.append('--offline')
metadata = json.loads(subprocess.check_output(command, cwd=root, encoding='utf-8'))
lock = tomllib.loads((root / 'Cargo.lock').read_text(encoding='utf-8'))
checksums = {(p['name'], p['version']): p.get('checksum') for p in lock['package']}
nodes = {n['id']: n for n in metadata['resolve']['nodes']}
refs = {p['id']: f"pkg:cargo/{p['name']}@{p['version']}" for p in metadata['packages']}
components, notices, missing = [], [], []
args.output.mkdir(parents=True, exist_ok=True)
licenses = args.output / 'licenses'
licenses.mkdir(exist_ok=True)
for package in sorted(metadata['packages'], key=lambda p: (p['name'], p['version'])):
    if package['id'] not in nodes or package['name'] == 'ssdownload':
        continue
    source = Path(package['manifest_path']).parent
    component = {'type': 'library', 'name': package['name'], 'version': package['version'],
                 'purl': refs[package['id']], 'bom-ref': refs[package['id']]}
    if package.get('license'):
        component['licenses'] = [{'expression': package['license']}]
    if package.get('repository'):
        component['externalReferences'] = [{'type': 'vcs', 'url': package['repository']}]
    checksum = checksums.get((package['name'], package['version']))
    if checksum:
        component['hashes'] = [{'alg': 'SHA-256', 'content': checksum}]
    components.append(component)
    found = []
    # Include vendored native-library notices as well as the crate's own license.
    for path in sorted(source.rglob('*')):
        if not path.is_file() or path.is_symlink():
            continue
        is_license = re.match(r'^(licen[cs]e|copying|notice|copyright|unlicense)([.\-_]|$)', path.name, re.I)
        if not is_license and str(path) != package.get('license_file'):
            continue
        relative = Path(f"{package['name']}-{package['version']}") / path.relative_to(source)
        target = licenses / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(path, target)
        found.append({'path': relative.as_posix(), 'sha256': hashlib.sha256(path.read_bytes()).hexdigest()})
    notices.append({'name': package['name'], 'version': package['version'], 'expression': package.get('license'), 'files': found})
    if not found:
        missing.append(f"{package['name']} {package['version']}")
nsis_license = args.nsis.parent / 'COPYING'
if not nsis_license.is_file():
    raise SystemExit('NSIS COPYING is required for installer distribution')
shutil.copyfile(nsis_license, licenses / 'NSIS-3.10-COPYING.txt')
rust_command = ['rustc'] + ([f'+{args.toolchain}'] if args.toolchain else [])
rust_details = subprocess.check_output(rust_command + ['--version', '--verbose'], cwd=root, encoding='utf-8')
release = re.search(r'^release: (\S+)$', rust_details, re.M)
if not release:
    raise SystemExit('rustc --version --verbose has no release field')
rust_version = release.group(1)
sysroot = Path(subprocess.check_output(rust_command + ['--print', 'sysroot'], cwd=root, encoding='utf-8').strip())
rust_notices = sysroot / 'share' / 'doc' / 'rust' / 'COPYRIGHT-library.html'
if not rust_notices.is_file():
    raise SystemExit('Rust standard-library COPYRIGHT-library.html is required')
shutil.copyfile(rust_notices, licenses / f'Rust-{rust_version}-COPYRIGHT-library.html')
crate_count = len(components)
components += [
    {'type': 'library', 'name': 'Rust standard library', 'version': rust_version,
     'bom-ref': f'rust-std-{rust_version}', 'licenses': [{'license': {'name': f'See Rust-{rust_version}-COPYRIGHT-library.html'}}]},
    {'type': 'library', 'name': 'NSIS installer stub and bundled installer components', 'version': '3.10',
     'bom-ref': 'nsis-3.10', 'licenses': [{'license': {'name': 'See NSIS-3.10-COPYING.txt'}}]}
]
app = next(p for p in metadata['packages'] if p['name'] == 'ssdownload')
bom = {'bomFormat': 'CycloneDX', 'specVersion': '1.6', 'version': 1,
       'metadata': {'component': {'type': 'application', 'name': app['name'], 'version': app['version'], 'bom-ref': refs[app['id']]},
                    'properties': [{'name': 'ssdownload:inventory-scope', 'value': 'Cargo.lock Windows target graph; includes build dependencies and bundled native-source license files. Optional downloaded media tools are inventoried separately at install time.'},
                                   {'name': 'ssdownload:installer-compiler', 'value': 'NSIS 3.10; COPYING retained'}]},
       'components': components,
       'dependencies': [{'ref': refs[key], 'dependsOn': sorted({refs[d['pkg']] for d in node['deps'] if d['pkg'] in nodes})} for key, node in sorted(nodes.items())]}
(args.output / 'sbom.cdx.json').write_text(json.dumps(bom, ensure_ascii=False, indent=2) + '\n', encoding='utf-8')
(licenses / 'index.json').write_text(json.dumps({'crates': notices, 'missing_texts': missing}, ensure_ascii=False, indent=2) + '\n', encoding='utf-8')
if missing:
    raise SystemExit('Missing original license texts: ' + ', '.join(missing))
print(f'Inventory: {crate_count} resolved crates plus Rust standard library and NSIS; original license texts copied.')

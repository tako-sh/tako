#!/usr/bin/python3 -I
"""Bounded root reconciler for Tako's native WireGuard networks."""

import base64
import binascii
import contextlib
import fcntl
import ipaddress
import json
import os
import pwd
import re
import secrets
import socket
import stat
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

NETWORKS = Path('/opt/tako/networks')
STATE = Path('/var/lib/tako-network-helper')
SSH = Path('/etc/ssh/tako-network')
SOCKET = Path('/run/tako-network-helper.sock')
TABLE = 'tako_network'
MAX_BYTES = 1024 * 1024
ENV = {'PATH': '/usr/sbin:/usr/bin:/sbin:/bin', 'LC_ALL': 'C'}


class Rejected(Exception):
    pass


def require(condition, message):
    if not condition:
        raise Rejected(message)


def network_id(value):
    require(isinstance(value, str) and re.fullmatch('[0-9a-f]{32}', value), 'invalid network_id')
    return value


def key(value):
    require(isinstance(value, str), 'invalid WireGuard key')
    try:
        raw = base64.b64decode(value, validate=True)
    except (ValueError, binascii.Error):
        raise Rejected('invalid WireGuard key') from None
    require(len(raw) == 32 and any(raw) and base64.b64encode(raw).decode() == value,
            'invalid WireGuard key')
    return value


def address(value):
    require(isinstance(value, str) and '%' not in value and value.endswith('/128'), 'expected ULA /128')
    try:
        ip = ipaddress.IPv6Interface(value)
    except ValueError:
        raise Rejected('expected ULA /128') from None
    require(ip.ip in ipaddress.IPv6Network('fc00::/7'), 'expected ULA /128')
    return str(ip)


def endpoint(value):
    require(isinstance(value, str) and len(value) <= 80 and '%' not in value, 'invalid endpoint')
    match = re.fullmatch(r'(?:\[([0-9a-fA-F:]+)\]|([0-9.]+)):([0-9]{1,5})', value)
    require(match is not None, 'expected public numeric endpoint')
    try:
        ip = ipaddress.ip_address(match[1] or match[2])
    except ValueError:
        raise Rejected('invalid endpoint') from None
    require(ip.is_global and not ip.is_multicast and not ip.is_reserved and
            not getattr(ip, 'ipv4_mapped', None) and 1 <= int(match[3]) <= 65535,
            'expected public numeric endpoint')
    return f'[{ip}]:{int(match[3])}' if ip.version == 6 else f'{ip}:{int(match[3])}'


def ssh_key(value):
    require(isinstance(value, str), 'invalid SSH key')
    parts = value.split(' ')
    require(len(parts) == 2 and parts[0] == 'ssh-ed25519', 'expected canonical Ed25519 SSH key')
    try:
        blob = base64.b64decode(parts[1], validate=True)
    except (ValueError, binascii.Error):
        raise Rejected('invalid SSH key') from None
    prefix = struct.pack('!I', 11) + b'ssh-ed25519' + struct.pack('!I', 32)
    require(len(blob) == len(prefix) + 32 and blob.startswith(prefix) and
            base64.b64encode(blob).decode() == parts[1], 'invalid SSH key')
    return value


def fields(data, required, optional=()):
    require(isinstance(data, dict) and set(required) <= data.keys() and
            data.keys() <= set(required) | set(optional), 'invalid configuration fields')


def device_id(value):
    require(isinstance(value, str) and re.fullmatch('[0-9a-f]{64}', value), 'invalid device_id')
    return value


def validate(data, ident):
    fields(data, ('network_id', 'device_id', 'address', 'private_key', 'peers'), ('listen_port',))
    require(network_id(data['network_id']) == ident, 'network_id mismatch')
    result = dict(data, address=address(data['address']), device_id=device_id(data['device_id']),
                  private_key=key(data['private_key']), listen_port=data.get('listen_port', 51820))
    require(type(result['listen_port']) is int and 1024 <= result['listen_port'] <= 65535 and
            result['listen_port'] != 8787, 'listen_port must be 1024..65535 except reserved gossip port 8787')
    require(isinstance(data['peers'], list) and len(data['peers']) <= 256, 'too many peers')
    prefix = ipaddress.IPv6Network(result['address'].split('/')[0] + '/48', strict=False)
    ids, addresses, keys = {result['device_id']}, {result['address']}, set()
    peers = []
    for peer in data['peers']:
        fields(peer, ('device_id', 'name', 'public_key', 'address', 'kind'),
               ('endpoint', 'ssh_user', 'ssh_public_key'))
        peer = dict(peer, device_id=device_id(peer['device_id']), address=address(peer['address']),
                    public_key=key(peer['public_key']))
        require(isinstance(peer['name'], str) and 1 <= len(peer['name']) <= 128 and
                all(c.isprintable() for c in peer['name']), 'invalid peer name')
        require(peer['kind'] in ('server', 'laptop'), 'invalid peer kind')
        require(ipaddress.IPv6Interface(peer['address']).ip in prefix, 'peer outside network /48')
        require(peer['device_id'] not in ids and peer['address'] not in addresses and
                peer['public_key'] not in keys, 'duplicate peer identity, address or key')
        ids.add(peer['device_id'])
        addresses.add(peer['address'])
        keys.add(peer['public_key'])
        if 'endpoint' in peer:
            peer['endpoint'] = endpoint(peer['endpoint'])
        require(('ssh_user' in peer) == ('ssh_public_key' in peer), 'SSH permission requires user and key')
        if 'ssh_user' in peer:
            require(peer['ssh_user'] == 'tako', 'SSH user must be tako')
            peer['ssh_public_key'] = ssh_key(peer['ssh_public_key'])
        peers.append(peer)
    result['peers'] = peers
    return result


def unique_object(pairs):
    result = {}
    for name, value in pairs:
        require(name not in result, 'duplicate JSON field')
        result[name] = value
    return result


@contextlib.contextmanager
def directory(path, owners=(0,)):
    fd = os.open('/', os.O_RDONLY | os.O_DIRECTORY)
    try:
        for part in Path(path).parts[1:]:
            child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            os.close(fd)
            fd = child
            info = os.fstat(fd)
            require(info.st_uid in owners and not info.st_mode & 0o022, 'unsafe directory ownership or mode')
        yield fd
    finally:
        os.close(fd)


def read_file(fd, name, owners=(0,), private=True):
    handle = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=fd)
    with os.fdopen(handle, 'rb') as stream:
        info = os.fstat(stream.fileno())
        require(stat.S_ISREG(info.st_mode) and info.st_nlink == 1 and info.st_uid in owners and
                not info.st_mode & (0o077 if private else 0o022) and info.st_size <= MAX_BYTES,
                'unsafe file ownership, mode, type or size')
        content = stream.read(MAX_BYTES + 1)
        require(len(content) <= MAX_BYTES, 'configuration too large')
        return content.decode('utf-8')


def read_desired(ident):
    uid = pwd.getpwnam('tako').pw_uid
    with directory(NETWORKS / ident, (0, uid)) as fd:
        return validate(json.loads(read_file(fd, 'desired.json', (0, uid)),
                                   object_pairs_hook=unique_object), ident)


def write_file(fd, name, content, mode=0o600):
    temporary = '.tmp-' + secrets.token_hex(16)
    handle = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, mode, dir_fd=fd)
    try:
        os.fchmod(handle, mode)
        with os.fdopen(handle, 'w') as stream:
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
        os.rename(temporary, name, src_dir_fd=fd, dst_dir_fd=fd)
        os.fsync(fd)
    finally:
        try:
            os.unlink(temporary, dir_fd=fd)
        except FileNotFoundError:
            pass


def serialize_state(state):
    content = json.dumps(state) + '\n'
    require(len(content.encode('utf-8')) <= MAX_BYTES, 'host state too large')
    return content


class Runner:
    def run(self, *args, input=None):
        try:
            result = subprocess.run(args, input=input, text=True, capture_output=True,
                                    env=ENV, cwd='/', timeout=30, check=False)
        except (OSError, subprocess.TimeoutExpired):
            raise Rejected(f'{args[0]} unavailable or timed out') from None
        require(result.returncode == 0, f'{args[0]} failed')
        return result.stdout

    def available(self, command):
        return any(os.access(Path(path) / command, os.X_OK) for path in ENV['PATH'].split(':'))


def interface(ident):
    return 'tako' + ident[:10]


def firewall(state):
    lines = [f'table inet {TABLE} {{', f'comment "{state["owner"]}"',
             'chain input { type filter hook input priority -10; policy accept;']
    # Drop gossip before generic established-connection rules in the host firewall.
    for ident, config in state['networks'].items():
        dev = interface(ident)
        local = config['address'].split('/')[0]
        lines.append(f'ip6 daddr {local} iifname != "{dev}" drop')
        for peer in config['peers']:
            ip = peer['address'].split('/')[0]
            match = f'iifname "{dev}" ip6 saddr {ip} ip6 daddr {local}'
            lines.append(f'{match} meta l4proto tcp ct state established ct direction reply accept')
            lines.append(f'{match} udp sport 53 ct state established ct direction reply accept')
            if peer['kind'] == 'server':
                lines.append(f'{match} udp dport 8787 accept')
                lines.append(f'{match} udp sport 8787 ct state established ct direction reply accept')
            if 'ssh_public_key' in peer:
                lines.append(f'{match} tcp dport 22 accept')
            lines.append(f'{match} tcp dport {{ 53, 80, 443, 9844 }} accept')
            lines.append(f'{match} udp dport 53 accept')
            lines.append(f'{match} meta l4proto ipv6-icmp accept')
        lines.append(f'iifname "{dev}" drop')
    lines.extend(['udp dport 8787 drop', '}',
                  'chain forward { type filter hook forward priority -10; policy accept;'])
    for ident in state['networks']:
        lines.extend([f'iifname "{interface(ident)}" drop', f'oifname "{interface(ident)}" drop'])
    lines.extend(['}', '}'])
    return '\n'.join(lines) + '\n'


def ssh_authorizations(state):
    lines = []
    for config in state['networks'].values():
        for peer in config['peers']:
            if 'ssh_public_key' in peer:
                ip = peer['address'].split('/')[0]
                lines.append(f'from="{ip}",no-agent-forwarding,no-X11-forwarding,no-port-forwarding '
                             f'{peer["ssh_public_key"]}')
    return '\n'.join(sorted(set(lines))) + ('\n' if lines else '')


class Host:
    def __init__(self, runner, state, save, write_ssh):
        self.runner, self.state, self.save, self.write_ssh = runner, state, save, write_ssh

    def owned_link(self, ident):
        links = json.loads(self.runner.run('ip', '-j', '-d', 'link', 'show'))
        link = next((link for link in links if link['ifname'] == interface(ident)), None)
        if link:
            previous = self.state['networks'].get(ident)
            require(previous is not None and link.get('ifalias') == previous['alias'] and
                    link.get('linkinfo', {}).get('info_kind') == 'wireguard', 'foreign interface')
        return link

    def firewall_exists(self):
        tables = json.loads(self.runner.run('nft', '-j', 'list', 'tables'))['nftables']
        exists = any(item.get('table', {}).get('name') == TABLE and
                     item['table'].get('family') == 'inet' for item in tables)
        if exists:
            rules = json.loads(self.runner.run('nft', '-j', 'list', 'table', 'inet', TABLE))['nftables']
            require(any(item.get('table', {}).get('comment') == self.state['owner'] for item in rules),
                    'foreign firewall table')
        return exists

    def install_firewall(self):
        exists = self.firewall_exists()
        script = (f'delete table inet {TABLE}\n' if exists else '') + firewall(self.state)
        self.runner.run('nft', '-f', '-', input=script)

    def check_routes(self, ident, desired):
        dev = interface(ident)
        wanted = {p['address'] for p in desired['peers']}
        routes = json.loads(self.runner.run('ip', '-j', '-N', '-6', 'route', 'show', 'table', 'main'))
        for route in routes:
            destination = route.get('dst', '')
            if ':' in destination and '/' not in destination:
                destination += '/128'
            managed = (route.get('dev') == dev and str(route.get('protocol')) == '242' and
                       route.get('metric') == 42760)
            link_local = (route.get('dev') == dev and destination == 'fe80::/64' and
                          str(route.get('protocol')) == '2')
            require(not (destination in wanted or route.get('dev') == dev) or managed or link_local,
                    'foreign route')
        return routes

    def open_host_firewall(self, ident, port):
        # These additive exceptions never delete a pre-existing administrator rule.
        config = self.state['networks'][ident]
        local = config['address'].split('/')[0]
        prefix = str(ipaddress.IPv6Network(local + '/48', strict=False))
        if self.runner.available('ufw'):
            status = self.runner.run('ufw', 'status')
            if status.startswith('Status: active'):
                self.runner.run('ufw', 'allow', f'{port}/udp', 'comment', 'tako-network-' + ident)
                self.runner.run('ufw', 'allow', 'in', 'on', interface(ident), 'from', prefix,
                                'to', local, 'comment', 'tako-network-' + ident)
        if self.runner.available('firewall-cmd'):
            # systemctl reports inactive with success via show, unlike firewall-cmd --state.
            active = self.runner.run('systemctl', 'show', 'firewalld', '--property=ActiveState', '--value').strip()
            if active == 'active':
                self.runner.run('firewall-cmd', '--add-port=' + str(port) + '/udp')
                self.runner.run('firewall-cmd', '--permanent', '--add-port=' + str(port) + '/udp')
                rule = f'rule family="ipv6" source address="{prefix}" destination address="{local}" accept'
                self.runner.run('firewall-cmd', '--add-rich-rule=' + rule)
                self.runner.run('firewall-cmd', '--permanent', '--add-rich-rule=' + rule)

    def apply(self, ident, desired):
        dev = interface(ident)
        require(all(other == ident or interface(other) != dev for other in self.state['networks']),
                'interface name collision')
        require(ident in self.state['networks'] or len(self.state['networks']) < 32, 'too many networks')
        for other, config in self.state['networks'].items():
            if other != ident:
                require(config['listen_port'] != desired['listen_port'], 'listen port already managed')
                require(ipaddress.IPv6Interface(config['address']).ip.packed[:6] !=
                        ipaddress.IPv6Interface(desired['address']).ip.packed[:6], 'network /48 already managed')
        link = self.owned_link(ident)
        for entry in self.runner.run('wg', 'show', 'all', 'listen-port').splitlines():
            other, port = entry.split()
            require(other == dev or int(port) != desired['listen_port'], 'WireGuard listen port in use')
        self.firewall_exists()
        routes = self.check_routes(ident, desired)
        previous = self.state['networks'].get(ident)
        requested = {desired['address']} | {peer['address'] for peer in desired['peers']}
        for item in json.loads(self.runner.run('ip', '-j', '-6', 'address', 'show')):
            if item['ifname'] != dev:
                require(all(a['local'] + '/128' not in requested for a in item.get('addr_info', [])),
                        'address belongs to another interface')
        addresses = []
        if link:
            addresses = json.loads(self.runner.run('ip', '-j', '-6', 'address', 'show', 'dev', dev))
            require(all(a['scope'] == 'link' or a['local'] + '/' + str(a['prefixlen']) in previous['managed_addresses']
                        for item in addresses for a in item.get('addr_info', [])), 'foreign interface address')
        public_key = self.runner.run('wg', 'pubkey', input=desired['private_key'] + '\n').strip()
        require(public_key not in {p['public_key'] for p in desired['peers']}, 'peer uses local WireGuard key')
        config = {k: v for k, v in desired.items() if k != 'private_key'}
        config['alias'] = previous['alias'] if previous else 'tako-network:' + ident + ':' + secrets.token_hex(16)
        config['managed_addresses'] = sorted(set((previous or {}).get('managed_addresses', []) + [desired['address']]))
        proposed = dict(self.state, networks={**self.state['networks'], ident: config})
        serialize_state(proposed)
        self.save(proposed)
        self.state = proposed
        owned = bool(link)
        try:
            self.install_firewall()
            self.write_ssh(ssh_authorizations(self.state))
            if not link:
                self.runner.run('ip', 'link', 'add', 'dev', dev, 'alias', config['alias'], 'type', 'wireguard')
                owned = True
            lines = ['[Interface]', 'PrivateKey = ' + desired['private_key'],
                     'ListenPort = ' + str(desired['listen_port'])]
            for peer in desired['peers']:
                lines.extend(['[Peer]', 'PublicKey = ' + peer['public_key'], 'AllowedIPs = ' + peer['address']])
                if 'endpoint' in peer:
                    lines.extend(['Endpoint = ' + peer['endpoint'], 'PersistentKeepalive = 25'])
            with tempfile.NamedTemporaryFile(mode='w', dir=STATE, prefix='.wg-', delete=True) as secret:
                os.chmod(secret.name, 0o600)
                secret.write('\n'.join(lines) + '\n')
                secret.flush()
                self.runner.run('wg', 'syncconf', dev, secret.name)
            for item in addresses:
                for entry in item.get('addr_info', []):
                    old = entry['local'] + '/' + str(entry['prefixlen'])
                    if entry['scope'] != 'link' and old != desired['address']:
                        self.runner.run('ip', '-6', 'address', 'del', old, 'dev', dev)
            self.runner.run('ip', '-6', 'address', 'replace', desired['address'], 'dev', dev, 'nodad', 'noprefixroute')
            self.runner.run('ip', 'link', 'set', 'dev', dev, 'up')
            wanted = {p['address'] for p in desired['peers']}
            for route in routes:
                if route.get('dev') == dev and str(route.get('protocol')) == '242':
                    destination = route['dst'] if '/' in route['dst'] else route['dst'] + '/128'
                    if destination not in wanted:
                        self.runner.run('ip', '-6', 'route', 'del', destination, 'dev', dev, 'proto', '242', 'metric', '42760')
            for destination in sorted(wanted):
                self.runner.run('ip', '-6', 'route', 'replace', destination, 'dev', dev, 'proto', '242', 'metric', '42760')
            self.open_host_firewall(ident, desired['listen_port'])
            self.runner.run('systemctl', 'enable', f'tako-network@{ident}.service')
            config['managed_addresses'] = [desired['address']]
            self.save(self.state)
        except Exception:
            if owned:
                self.runner.run('ip', 'link', 'set', 'dev', dev, 'down')
            raise
        return {'address': desired['address'].split('/')[0],
                'applied_device_ids': [p['device_id'] for p in desired['peers']]}

    def remove(self, ident):
        link = self.owned_link(ident)
        self.firewall_exists()
        if ident not in self.state['networks']:
            return {'applied_device_ids': []}
        if link:
            self.runner.run('ip', 'link', 'set', 'dev', interface(ident), 'down')
            self.runner.run('ip', 'link', 'delete', 'dev', interface(ident))
        self.runner.run('systemctl', 'disable', f'tako-network@{ident}.service')
        del self.state['networks'][ident]
        self.write_ssh(ssh_authorizations(self.state))
        self.install_firewall()
        self.save(self.state)
        return {'applied_device_ids': []}


def operation(args):
    require(os.geteuid() == 0, 'root required')
    ident, action = request_args(args)
    os.umask(0o077)
    with directory(STATE) as fd, directory(SSH) as sshfd:
        lock = os.open('lock', os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600, dir_fd=fd)
        with os.fdopen(lock, 'w') as stream:
            info = os.fstat(stream.fileno())
            require(stat.S_ISREG(info.st_mode) and info.st_uid == 0 and info.st_nlink == 1 and
                    not info.st_mode & 0o077, 'unsafe lock')
            fcntl.flock(stream, fcntl.LOCK_EX)
            try:
                state = json.loads(read_file(fd, 'state.json'))
            except FileNotFoundError:
                state = {'owner': 'tako-network-' + secrets.token_hex(16), 'networks': {}}
            host = Host(Runner(), state,
                        lambda value: write_file(fd, 'state.json', serialize_state(value)),
                        lambda value: write_file(sshfd, 'authorized_keys', value, 0o644))
            result = host.apply(ident, read_desired(ident)) if action == 'apply' else host.remove(ident)
    return dict(ok=True, action=action, network_id=ident, interface=interface(ident), **result)


def request_args(args):
    require(len(args) == 2 and args[1] in ('apply', 'remove'), 'expected NETWORK_ID apply|remove')
    return network_id(args[0]), args[1]


def failure(error):
    # Never echo parser errors, subprocess stderr, or input containing private keys.
    return {'ok': False, 'error': str(error) if isinstance(error, Rejected) else 'invalid or unavailable host state'}


def peer_uid(connection):
    return struct.unpack('3i', connection.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))[1]


def receive(connection, limit):
    data = bytearray()
    while True:
        part = connection.recv(min(4096, limit + 1 - len(data)))
        if not part:
            return bytes(data)
        data.extend(part)
        require(len(data) <= limit, 'request or response too large')


def handle_connection(connection, tako_uid):
    try:
        connection.settimeout(5)
        require(peer_uid(connection) in (0, tako_uid), 'unauthorized local caller')
        request = receive(connection, 40)
        require(re.fullmatch(rb'[0-9a-f]{32} (apply|remove)\n', request), 'invalid request frame')
        result = operation(request.decode('ascii').strip().split(' '))
    except Exception as error:
        result = failure(error)
    connection.sendall((json.dumps(result) + '\n').encode())


def serve():
    require(os.geteuid() == 0, 'root required')
    require(os.environ.get('LISTEN_PID') == str(os.getpid()) and os.environ.get('LISTEN_FDS') == '1',
            'systemd socket activation required')
    tako_uid = pwd.getpwnam('tako').pw_uid
    with socket.socket(fileno=3) as listener:
        require(listener.family == socket.AF_UNIX and listener.getsockname() == str(SOCKET) and
                listener.getsockopt(socket.SOL_SOCKET, socket.SO_ACCEPTCONN), 'invalid activation socket')
        while True:
            connection, _ = listener.accept()
            with connection:
                try:
                    handle_connection(connection, tako_uid)
                except OSError:
                    pass  # A disconnected caller does not undo a reconciled network.


def request_operation(args):
    ident, action = request_args(args)
    info = os.lstat(SOCKET)
    require(stat.S_ISSOCK(info.st_mode) and info.st_uid == 0 and not info.st_mode & 0o007,
            'unsafe helper socket')
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
        connection.settimeout(900)
        connection.connect(str(SOCKET))
        require(peer_uid(connection) == 0, 'untrusted helper service')
        connection.sendall(f'{ident} {action}\n'.encode('ascii'))
        connection.shutdown(socket.SHUT_WR)
        result = json.loads(receive(connection, MAX_BYTES))
        require(isinstance(result, dict) and type(result.get('ok')) is bool, 'invalid helper response')
        return result


def main(args):
    try:
        if args == ['--serve']:
            serve()
            return 0
        result = request_operation(args)
    except Exception as error:
        result = failure(error)
    print(json.dumps(result))
    return 0 if result['ok'] else 1


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))

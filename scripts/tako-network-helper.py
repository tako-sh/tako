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
    require(match is not None, 'expected unicast numeric endpoint')
    try:
        ip = ipaddress.ip_address(match[1] or match[2])
    except ValueError:
        raise Rejected('invalid endpoint') from None
    private = any(ip in ipaddress.ip_network(prefix) for prefix in (
        ('10.0.0.0/8', '172.16.0.0/12', '192.168.0.0/16', '100.64.0.0/10')
        if ip.version == 4 else ('fc00::/7',)))
    require((ip.is_global or private) and not ip.is_multicast and not ip.is_loopback and
            not ip.is_unspecified and not ip.is_link_local and not ip.is_reserved and
            not getattr(ip, 'ipv4_mapped', None) and 1 <= int(match[3]) <= 65535,
            'expected unicast numeric endpoint')
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
    fields(data, ('network_id', 'device_id', 'address', 'private_key', 'peers'), ('listen_port', 'ssh_port'))
    require(network_id(data['network_id']) == ident, 'network_id mismatch')
    result = dict(data, address=address(data['address']), device_id=device_id(data['device_id']),
                  private_key=key(data['private_key']), listen_port=data.get('listen_port', 51820))
    require(type(result['listen_port']) is int and (result['listen_port'] == 443 or 1024 <= result['listen_port'] <= 65535) and
            result['listen_port'] != 8787, 'listen_port must be 443 or 1024..65535 except reserved gossip port 8787')
    result['ssh_port'] = data.get('ssh_port', 22)
    require(type(result['ssh_port']) is int and 1 <= result['ssh_port'] <= 65535 and
            result['ssh_port'] not in (53, 80, 443, 9844, 9850), 'invalid or reserved SSH port')
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
            endpoint_ip = peer['endpoint'].rsplit(':', 1)[0].strip('[]')
            require(ipaddress.ip_address(endpoint_ip) not in prefix, 'endpoint overlaps WireGuard network')
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
            pass  # The rename above already consumed the temporary file.


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
        require(len(result.stdout.encode()) <= MAX_BYTES, 'host command response too large')
        return result.stdout

    def available(self, command):
        return any(os.access(Path(path) / command, os.X_OK) for path in ENV['PATH'].split(':'))


def interface(ident):
    return 'tako' + ident[:10]


def route_type(route):
    # ip -N reports RTN_* values numerically and omits the default unicast type.
    value = str(route.get('type', 'unicast'))
    return {'1': 'unicast', '2': 'local', '3': 'broadcast', '5': 'multicast'}.get(value, value)


def bridge_config(ident):
    network_id(ident)
    raw = bytes.fromhex(ident)
    # Stay inside 10.64.0.0/11: clear of the ranges clouds and container
    # platforms hand out by default (10.0.0.0/16, 10.42-43.0.0/16, 10.96.0.0/12,
    # 10.128.0.0/9), which the host route check would otherwise refuse.
    second, third = 64 + raw[6] % 32, raw[7]
    return {'name': 'tako-svc-' + ident[:12], 'interface': 'tks' + ident[:12],
            'subnet': f'10.{second}.{third}.0/24', 'gateway': f'10.{second}.{third}.1'}


def bridge_options(ident):
    return {'com.docker.network.bridge.name': bridge_config(ident)['interface'],
            'com.docker.network.bridge.enable_icc': 'false',
            'com.docker.network.bridge.enable_ip_masquerade': 'false'}


def firewall(state):
    lines = [f'table inet {TABLE} {{', f'comment "{state["owner"]}"',
             'chain input { type filter hook input priority -10; policy accept;']
    # Drop gossip before generic established-connection rules in the host firewall.
    for ident, config in state['networks'].items():
        dev = interface(ident)
        local = config['address'].split('/')[0]
        local_match = f'iifname "lo" ip6 saddr {local} ip6 daddr {local}'
        lines.append(f'{local_match} tcp dport {{ 53, 80, 443, 9844 }} accept')
        lines.append(f'{local_match} udp dport 53 accept')
        lines.append(f'{local_match} ct state established ct direction reply accept')
        lines.append(f'ip6 daddr {local} iifname != "{dev}" drop')
        for peer in config['peers']:
            ip = peer['address'].split('/')[0]
            match = f'iifname "{dev}" ip6 saddr {ip} ip6 daddr {local}'
            lines.append(f'{match} meta l4proto tcp ct state established ct direction reply accept')
            lines.append(f'{match} udp sport 53 ct state established ct direction reply accept')
            if peer['kind'] == 'server':
                lines.append(f'{match} udp dport 8787 accept')
                lines.append(f'{match} udp sport 8787 ct state established ct direction reply accept')
                lines.append(f'{match} tcp dport 9850 accept')
            if 'ssh_public_key' in peer:
                lines.append(f'{match} tcp dport {config.get("ssh_port", 22)} accept')
            lines.append(f'{match} tcp dport {{ 53, 80, 443, 9844 }} accept')
            lines.append(f'{match} udp dport 53 accept')
            lines.append(f'{match} meta l4proto ipv6-icmp accept')
        lines.append(f'iifname "{dev}" drop')
        if config.get('compose_bridge'):
            bridge = bridge_config(ident)
            name, gateway, subnet = bridge['interface'], bridge['gateway'], bridge['subnet']
            lines.append(f'ip daddr {gateway} iifname != "{name}" drop')
            match = f'iifname "{name}" ip saddr {subnet} ip daddr {gateway}'
            lines.append(f'{match} tcp dport {{ 53, 80 }} accept')
            lines.append(f'{match} udp dport 53 accept')
            lines.append(f'iifname "{name}" drop')
    lines.extend(['udp dport 8787 drop', 'tcp dport 9850 drop', '}',
                  'chain forward { type filter hook forward priority -10; policy accept;'])
    for ident in state['networks']:
        lines.extend([f'iifname "{interface(ident)}" drop', f'oifname "{interface(ident)}" drop'])
        if state['networks'][ident].get('compose_bridge'):
            bridge = bridge_config(ident)['interface']
            lines.extend([f'iifname "{bridge}" drop', f'oifname "{bridge}" drop'])
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
        links = json.loads(self.runner.run('ip', '-j', '-N', '-d', 'link', 'show'))
        link = next((link for link in links if link['ifname'] == interface(ident)), None)
        if link:
            previous = self.state['networks'].get(ident)
            require(previous and link.get('linkinfo', {}).get('info_kind') == 'wireguard' and (
                link.get('ifalias') == previous['alias'] or
                (not link.get('ifalias') and 'creation_group' in previous and
                 str(link.get('group')) == str(previous['creation_group']))), 'foreign interface')
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

    def inspect_host_state(self, ident, link, desired):
        dev = interface(ident)
        previous = self.state['networks'].get(ident, {})
        owned_addresses = set(previous.get('managed_addresses', []))
        owned_routes = {p['address'] for p in previous.get('peers', [])} | set(previous.get('retiring_routes', []))
        addresses = []
        local_addresses = set(owned_addresses)
        if link:
            addresses = json.loads(self.runner.run('ip', '-j', 'address', 'show', 'dev', dev))
            for item in addresses:
                for entry in item.get('addr_info', []):
                    ip = ipaddress.ip_address(entry['local'])
                    cidr = f'{ip}/{entry["prefixlen"]}'
                    kernel_link_local = ip.version == 6 and ip.is_link_local and entry['scope'] == 'link' and entry['prefixlen'] == 64
                    require(ip.version == 6 and (cidr in owned_addresses or kernel_link_local),
                            'foreign interface address')
                    local_addresses.add(f'{ip}/128')
        nexthops = json.loads(self.runner.run('ip', '-j', '-N', 'nexthop', 'show'))
        require(all(item.get('dev') != dev for item in nexthops), 'foreign nexthop')
        wanted = {p['address'] for p in desired['peers']}
        managed_routes = []
        for family in ('-4', '-6'):
            routes = json.loads(self.runner.run('ip', '-j', '-N', family, 'route', 'show', 'table', 'all'))
            for route in routes:
                destination = route.get('dst', '')
                if family == '-6' and ':' in destination and '/' not in destination:
                    destination += '/128'
                table = str(route.get('table', '254'))
                main_table = table in ('254', 'main')
                touches_interface = (route.get('dev') == dev or route.get('iif') == dev or
                                     any(hop.get('dev') == dev for hop in route.get('nexthops', [])))
                if not touches_interface:
                    require(not (family == '-6' and main_table and destination in wanted), 'foreign route')
                    continue
                simple = route.get('dev') == dev and not any(k in route for k in ('gateway', 'via', 'encap', 'nexthops', 'nhid', 'iif'))
                kind = route_type(route)
                managed = (simple and family == '-6' and main_table and destination in owned_routes and
                           str(route.get('protocol')) == '242' and route.get('metric') == 42760 and
                           kind == 'unicast')
                kernel = simple and family == '-6' and str(route.get('protocol')) == '2'
                kernel_link_local = kernel and main_table and destination == 'fe80::/64' and kind == 'unicast'
                kernel_local = kernel and table in ('255', 'local') and (
                    (kind == 'local' and destination in local_addresses) or
                    (kind == 'multicast' and destination == 'ff00::/8'))
                require(managed or kernel_link_local or kernel_local, 'foreign route')
                if managed:
                    managed_routes.append(route)
        return managed_routes, addresses

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

    def docker(self, *args):
        require(self.runner.available('docker'), 'Docker is required for native Compose applications')
        return self.runner.run('docker', '--host', 'unix:///var/run/docker.sock',
                               '--config', str(STATE / 'docker'), 'network', *args)

    def docker_networks(self):
        ids = self.docker('ls', '--quiet', '--no-trunc').split()
        require(len(ids) <= 256 and len(ids) == len(set(ids)) and
                all(re.fullmatch('[0-9a-f]{12,64}', value) for value in ids), 'invalid Docker network inventory')
        if not ids:
            return []
        networks = json.loads(self.docker('inspect', *ids))
        require(isinstance(networks, list) and len(networks) == len(ids) and
                {item.get('Id') for item in networks} == set(ids), 'incomplete Docker network inventory')
        return networks

    def bridge_network(self, ident, networks):
        expected = bridge_config(ident)
        matches = [n for n in networks if n.get('Name') == expected['name']]
        require(len(matches) <= 1, 'ambiguous native bridge')
        for network in networks:
            if network in matches:
                continue
            for ipam in network.get('IPAM', {}).get('Config') or []:
                subnet = ipaddress.ip_network(ipam['Subnet'], strict=False)
                require(not subnet.overlaps(ipaddress.ip_network(expected['subnet'])), 'Docker subnet collision')
        if not matches:
            return None
        network = matches[0]
        labels = network.get('Labels') or {}
        require(labels.get('sh.tako.network') == ident and labels.get('sh.tako.owner') == self.state['owner'],
                'foreign Docker network')
        require(network.get('Driver') == 'bridge' and network.get('Scope') == 'local' and
                network.get('Internal') is True and network.get('EnableIPv6') is False and
                network.get('EnableIPv4', True) is True and not network.get('Ingress') and
                network.get('Options') == bridge_options(ident), 'native bridge settings changed')
        ipam = network.get('IPAM', {})
        require(ipam.get('Driver') == 'default' and not ipam.get('Options') and
                ipam.get('Config') == [{'Subnet': expected['subnet'], 'Gateway': expected['gateway']}],
                'native bridge IPAM changed')
        return network

    def inspect_bridge_host(self, ident, network):
        expected = bridge_config(ident)
        subnet = ipaddress.ip_network(expected['subnet'])
        dev = expected['interface']
        links = json.loads(self.runner.run('ip', '-j', '-d', 'link', 'show'))
        link = next((link for link in links if link['ifname'] == dev), None)
        require((link is None and network is None) or (link is not None and network is not None and
                link.get('linkinfo', {}).get('info_kind') == 'bridge'), 'foreign or missing native bridge interface')
        has_gateway = False
        for interface_state in json.loads(self.runner.run('ip', '-j', 'address', 'show')):
            for address_state in interface_state.get('addr_info', []):
                ip = ipaddress.ip_interface(f'{address_state["local"]}/{address_state["prefixlen"]}')
                if interface_state['ifname'] == dev:
                    owned = network is not None and (
                        (str(ip.ip) == expected['gateway'] and ip.network == subnet) or
                        (ip.version == 6 and ip.ip.is_link_local and ip.network.prefixlen == 64))
                    require(owned, 'foreign native bridge address')
                    has_gateway |= str(ip.ip) == expected['gateway']
                else:
                    require(not ip.network.overlaps(subnet), 'host address overlaps native bridge')
        require(network is None or has_gateway, 'native bridge gateway is missing')
        routes = json.loads(self.runner.run('ip', '-j', '-N', '-4', 'route', 'show', 'table', 'all'))
        for route in routes:
            destination = route.get('dst', 'default')
            touches = route.get('dev') == dev or route.get('iif') == dev or any(
                hop.get('dev') == dev for hop in route.get('nexthops', []))
            if destination == 'default':
                require(not touches, 'foreign default route on native bridge')
                continue
            prefix = ipaddress.ip_network(destination, strict=False)
            if not prefix.overlaps(subnet) and not touches:
                continue
            owned = network is not None and route.get('dev') == dev and prefix.subnet_of(subnet)
            owned = (owned and str(route.get('protocol')) == '2' and
                     str(route.get('table', '254')) in ('254', 'main', '255', 'local') and
                     route_type(route) in ('unicast', 'local', 'broadcast') and
                     not any(field in route for field in ('gateway', 'via', 'encap', 'nexthops', 'nhid', 'iif')))
            require(owned, 'host route overlaps native bridge')

    def open_bridge_firewall(self, ident):
        config = bridge_config(ident)
        if self.runner.available('ufw') and self.runner.run('ufw', 'status').startswith('Status: active'):
            for port, protocol in ((80, 'tcp'), (53, 'tcp'), (53, 'udp')):
                self.runner.run('ufw', 'allow', 'in', 'on', config['interface'], 'from', config['subnet'],
                                'to', config['gateway'], 'port', str(port), 'proto', protocol,
                                'comment', 'tako-network-' + ident)
        if self.runner.available('firewall-cmd'):
            active = self.runner.run('systemctl', 'show', 'firewalld', '--property=ActiveState', '--value').strip()
            if active == 'active':
                for port, protocol in ((80, 'tcp'), (53, 'tcp'), (53, 'udp')):
                    rule = (f'rule family="ipv4" source address="{config["subnet"]}" '
                            f'destination address="{config["gateway"]}" port port="{port}" protocol="{protocol}" accept')
                    self.runner.run('firewall-cmd', '--add-rich-rule=' + rule)
                    self.runner.run('firewall-cmd', '--permanent', '--add-rich-rule=' + rule)

    def bridge(self, ident):
        require(ident in self.state['networks'] and self.owned_link(ident) is not None,
                'native WireGuard link is not ready')
        self.firewall_exists()
        expected = bridge_config(ident)
        network = self.bridge_network(ident, self.docker_networks())
        self.inspect_bridge_host(ident, network)
        self.state['networks'][ident]['compose_bridge'] = True
        self.save(self.state)
        self.install_firewall()
        if network is None:
            options = [item for key, value in bridge_options(ident).items() for item in ('--opt', key + '=' + value)]
            self.docker('create', '--driver', 'bridge', '--internal', '--subnet', expected['subnet'],
                        '--gateway', expected['gateway'], '--label', 'sh.tako.network=' + ident,
                        '--label', 'sh.tako.owner=' + self.state['owner'], *options, expected['name'])
            network = self.bridge_network(ident, self.docker_networks())
            require(network is not None, 'native bridge creation was not acknowledged')
            self.inspect_bridge_host(ident, network)
        self.open_bridge_firewall(ident)
        return expected

    def remove_bridge(self, ident):
        if not self.state['networks'][ident].get('compose_bridge'):
            return
        network = self.bridge_network(ident, self.docker_networks())
        self.inspect_bridge_host(ident, network)
        if network is not None:
            require(not network.get('Containers'), 'stop Compose containers before removing their native network')
            self.docker('rm', network['Id'])

    def dns_values(self, dev, setting):
        text = self.runner.run('resolvectl', setting, dev).strip()
        heading, separator, value = text.partition(':')
        require(separator and re.fullmatch(r'Link [0-9]+ \(' + re.escape(dev) + r'\)', heading),
                'unsupported per-link resolved response')
        values = value.split()
        require(len(values) <= 32 and all(len(v) <= 255 and not v.startswith('-') and
                re.fullmatch(r'[A-Za-z0-9:.%#_\[\]~+-]+', v) for v in values),
                'unsupported per-link resolved values')
        if setting == 'default-route':
            require(values in (['yes'], ['no']), 'unsupported resolved default route')
        return values

    def configure_dns(self, ident, config):
        mutating = False
        if not self.runner.available('resolvectl'):
            return {'dns_supported': False, 'dns_error': 'native service DNS requires systemd-resolved; resolver configuration was not changed'}
        try:
            active = self.runner.run('systemctl', 'show', 'systemd-resolved.service',
                                     '--property=ActiveState', '--value').strip()
            require(active == 'active', 'native service DNS requires active systemd-resolved')
            dev = interface(ident)
            installed = {'dns': [config['address'].split('/')[0]],
                         'domain': ['~' + ident + '.tako.internal'], 'default-route': ['no']}
            current = {setting: self.dns_values(dev, setting) for setting in installed}
            previous = config.get('dns_settings')
            if previous:
                for setting, value in current.items():
                    baseline = ['yes'] if setting == 'default-route' else []
                    require(value in (previous['installed'][setting], previous['original'][setting], baseline),
                            'per-link DNS settings changed outside Tako')
            config['dns_settings'] = {'original': previous['original'] if previous else current,
                                      'installed': installed}
            self.save(self.state)
            mutating = True
            for setting, values in installed.items():
                self.runner.run('resolvectl', setting, dev, *values)
            return {'dns_supported': True}
        except Rejected as error:
            diagnostic = str(error)
            if mutating:
                try:
                    self.restore_dns(ident, config)
                except Rejected:
                    diagnostic += '; per-link DNS rollback is incomplete'
            return {'dns_supported': False, 'dns_error': diagnostic}

    def restore_dns(self, ident, config):
        record = config.get('dns_settings')
        if not record or not self.runner.available('resolvectl'):
            return
        active = self.runner.run('systemctl', 'show', 'systemd-resolved.service',
                                 '--property=ActiveState', '--value').strip()
        if active != 'active':
            return
        for setting, installed in record['installed'].items():
            if self.dns_values(interface(ident), setting) == installed:
                original = record['original'][setting]
                self.runner.run('resolvectl', setting, interface(ident), *(original or ['']))

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
        routes, addresses = self.inspect_host_state(ident, link, desired)
        previous = self.state['networks'].get(ident)
        requested = {desired['address']} | {peer['address'] for peer in desired['peers']}
        for item in json.loads(self.runner.run('ip', '-j', '-6', 'address', 'show')):
            if item['ifname'] != dev:
                require(all(a['local'] + '/128' not in requested for a in item.get('addr_info', [])),
                        'address belongs to another interface')
        public_key = self.runner.run('wg', 'pubkey', input=desired['private_key'] + '\n').strip()
        require(public_key not in {p['public_key'] for p in desired['peers']}, 'peer uses local WireGuard key')
        config = {k: v for k, v in desired.items() if k != 'private_key'}
        for retained in ('dns_settings', 'compose_bridge', 'creation_group'):
            if previous and retained in previous:
                config[retained] = previous[retained]
        config['alias'] = previous['alias'] if previous else 'tako-network:' + ident + ':' + secrets.token_hex(16)
        if not link:
            # Linux ignores alias on link creation. Journal a random group that
            # is set atomically with creation, so a crash before tagging is recoverable.
            config['creation_group'] = 1 + secrets.randbelow(2**31 - 1)
        config['managed_addresses'] = sorted(set((previous or {}).get('managed_addresses', []) + [desired['address']]))
        old_routes = {p['address'] for p in (previous or {}).get('peers', [])} | set((previous or {}).get('retiring_routes', []))
        config['retiring_routes'] = sorted(old_routes - {p['address'] for p in desired['peers']})
        proposed = dict(self.state, networks={**self.state['networks'], ident: config})
        serialize_state(proposed)
        self.save(proposed)
        self.state = proposed
        owned = bool(link)
        try:
            self.install_firewall()
            self.write_ssh(ssh_authorizations(self.state))
            if not link:
                self.runner.run('ip', 'link', 'add', 'dev', dev, 'group', str(config['creation_group']), 'type', 'wireguard')
                owned = True
            if 'creation_group' in config:
                self.runner.run('ip', 'link', 'set', 'dev', dev, 'alias', config['alias'], 'group', '0')
                del config['creation_group']
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
            dns_result = self.configure_dns(ident, config)
            config['managed_addresses'] = [desired['address']]
            config['retiring_routes'] = []
            self.save(self.state)
        except Exception:
            if owned:
                self.runner.run('ip', 'link', 'set', 'dev', dev, 'down')
            raise
        return {'address': desired['address'].split('/')[0],
                'applied_device_ids': [p['device_id'] for p in desired['peers']], **dns_result}

    def remove(self, ident):
        link = self.owned_link(ident)
        self.firewall_exists()
        if ident not in self.state['networks']:
            return {'applied_device_ids': []}
        self.inspect_host_state(ident, link, self.state['networks'][ident])
        self.remove_bridge(ident)
        if link:
            self.restore_dns(ident, self.state['networks'][ident])
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
            if action in ('apply', 'isolate'):
                desired = read_desired(ident)
                if action == 'isolate':
                    desired['peers'] = []
                result = host.apply(ident, desired)
            elif action == 'bridge':
                with directory(STATE / 'docker'):
                    result = host.bridge(ident)
            else:
                if state['networks'].get(ident, {}).get('compose_bridge'):
                    with directory(STATE / 'docker'):
                        result = host.remove(ident)
                else:
                    result = host.remove(ident)
    return dict(result, ok=True, action=action, network_id=ident, interface=interface(ident))


def request_args(args):
    require(len(args) == 2 and args[1] in ('apply', 'isolate', 'remove', 'bridge'), 'expected NETWORK_ID apply|isolate|remove|bridge')
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
        request = receive(connection, 41)
        require(re.fullmatch(rb'[0-9a-f]{32} (apply|isolate|remove|bridge)\n', request), 'invalid request frame')
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

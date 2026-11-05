import base64
import copy
import contextlib
import io
import importlib.util
import json
import os
from pathlib import Path
import struct
import tempfile
import unittest
from unittest.mock import patch


SOURCE = Path(__file__).resolve().parents[1] / 'tako-network-helper.py'
spec = importlib.util.spec_from_file_location('network_helper', SOURCE)
helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)
ID = 'ab' * 16
DEV = helper.interface(ID)
KEY = base64.b64encode(bytes(range(32))).decode()
PEER_KEY = base64.b64encode(bytes(range(1, 33))).decode()
SSH_KEY = 'ssh-ed25519 ' + base64.b64encode(
    struct.pack('!I', 11) + b'ssh-ed25519' + struct.pack('!I', 32) + bytes(range(32))).decode()


def desired():
    return {'network_id': ID, 'device_id': '12' * 32, 'address': 'fd00:1234:5678::1/128',
            'private_key': KEY, 'peers': [
                {'device_id': '34' * 32, 'name': 'laptop', 'public_key': PEER_KEY,
                 'address': 'fd00:1234:5678::2/128', 'kind': 'laptop',
                 'endpoint': '8.8.8.8:51820', 'ssh_user': 'tako', 'ssh_public_key': SSH_KEY}]}


class ValidationTests(unittest.TestCase):
    def test_defaults_and_canonicalization(self):
        value = helper.validate(desired(), ID)
        self.assertEqual(value['listen_port'], 51820)
        self.assertEqual(helper.interface(ID), DEV)
        self.assertLessEqual(len(DEV), 15)

    def test_unsafe_network_input(self):
        cases = [
            ('network_id', '../' + ID), ('network_id', ID.upper()),
            ('device_id', 'x' * 64), ('address', '::/0'), ('address', '8.8.8.8/32'),
            ('address', '2001:4860::1/128'), ('address', 'fd00::1%eth0/128'),
            ('address', 'fd00::/48'), ('private_key', 'x\nPostUp = reboot'),
            ('private_key', base64.b64encode(bytes(32)).decode()),
            ('listen_port', True), ('listen_port', 0), ('listen_port', 65536), ('listen_port', 8787),
            ('ssh_port', True), ('ssh_port', 0), ('ssh_port', 65536), ('ssh_port', 80),
            ('listen_port', '51820\n'), ('PostUp', 'reboot'), ('routes', ['::/0']),
            ('peers', {}), ('peers', [desired()['peers'][0]] * 257),
        ]
        for field, value in cases:
            with self.subTest(field=field, value=value):
                data = desired()
                data[field] = value
                with self.assertRaises(helper.Rejected):
                    helper.validate(data, ID)

    def test_unsafe_peer_input(self):
        for field, value in [
            ('address', 'fd01::2/128'), ('address', 'fd00:1234:5678::1/128'),
            ('public_key', KEY + '\n'), ('device_id', '12' * 32), ('kind', 'admin'),
            ('name', 'hello\nworld'), ('ssh_user', 'root'),
            ('ssh_public_key', SSH_KEY + ' comment'), ('ssh_public_key', 'ssh-ed25519 AAAA'),
            ('ssh_public_key', 'command="sh" ' + SSH_KEY), ('allowed_ips', ['::/0']),
            ('endpoint', 'localhost:51820'), ('endpoint', '127.0.0.1:80'),
            ('endpoint', '169.254.169.254:80'), ('endpoint', '[fd00:1234:5678::2]:51820'),
            ('endpoint', '[fe80::1]:80'), ('endpoint', '[::1]:80'),
            ('endpoint', '224.0.0.1:80'), ('endpoint', '8.8.8.8:0'),
            ('endpoint', '8.8.8.8:80\nPostUp=sh'),
        ]:
            with self.subTest(field=field, value=value):
                data = desired()
                data['peers'][0][field] = value
                with self.assertRaises(helper.Rejected):
                    helper.validate(data, ID)

    def test_duplicate_peers_and_incomplete_ssh_grants(self):
        data = desired()
        data['peers'] *= 2
        with self.assertRaises(helper.Rejected):
            helper.validate(data, ID)
        for field in ('ssh_user', 'ssh_public_key'):
            data = desired()
            del data['peers'][0][field]
            with self.assertRaises(helper.Rejected):
                helper.validate(data, ID)

    def test_duplicate_json_fields_rejected(self):
        with self.assertRaises(helper.Rejected):
            json.loads('{"a":1,"a":2}', object_pairs_hook=helper.unique_object)

    def test_cli_rejects_extra_arguments_and_unknown_actions_as_json(self):
        for args in ([ID, 'apply', '/tmp/payload'], [ID, 'shell'], ['../etc', 'remove'], []):
            output = io.StringIO()
            with patch.object(helper.os, 'geteuid', return_value=0), contextlib.redirect_stdout(output):
                code = helper.main(args)
            self.assertNotEqual(code, 0)
            self.assertFalse(json.loads(output.getvalue())['ok'])
            self.assertNotIn('/tmp/payload', output.getvalue())

    def test_private_vpc_endpoints_and_exact_ssh_port(self):
        for value in ('10.0.0.1:51820', '172.16.1.2:51821', '192.168.2.4:51820',
                      '100.64.1.2:51820', '[fd12:3456::2]:51820'):
            self.assertEqual(helper.endpoint(value), value)
        data = desired()
        data['ssh_port'] = 2222
        config = helper.validate(data, ID)
        policy = helper.firewall({'owner': 'test', 'networks': {ID: config}})
        self.assertIn('tcp dport 2222 accept', policy)
        self.assertNotIn('tcp dport 22 accept', policy)

    def test_ipv6_public_endpoint(self):
        self.assertEqual(helper.endpoint('[2606:4700:4700::1111]:51820'),
                         '[2606:4700:4700::1111]:51820')


class FilesystemTests(unittest.TestCase):
    def test_refuses_symlink_hardlink_nonprivate_and_fifo_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / 'secret').write_text('secret')
            (root / 'secret').chmod(0o600)
            (root / 'symlink').symlink_to(root / 'secret')
            os.link(root / 'secret', root / 'hardlink')
            (root / 'public').write_text('secret')
            (root / 'public').chmod(0o644)
            os.mkfifo(root / 'fifo')
            fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY)
            try:
                for name in ('symlink', 'hardlink', 'secret', 'public', 'fifo'):
                    with self.subTest(name=name), self.assertRaises((helper.Rejected, OSError)):
                        helper.read_file(fd, name, (os.getuid(),))
            finally:
                os.close(fd)

    def test_bounded_read_and_atomic_write(self):
        with tempfile.TemporaryDirectory() as tmp:
            fd = os.open(tmp, os.O_RDONLY | os.O_DIRECTORY)
            try:
                helper.write_file(fd, 'desired.json', '{}')
                self.assertEqual(helper.read_file(fd, 'desired.json', (os.getuid(),)), '{}')
                helper.write_file(fd, 'desired.json', 'x' * (helper.MAX_BYTES + 1))
                with self.assertRaises(helper.Rejected):
                    helper.read_file(fd, 'desired.json', (os.getuid(),))
            finally:
                os.close(fd)


class FakeRunner:
    def __init__(self):
        self.calls = []
        self.links = []
        self.routes = []
        self.addresses = []
        self.table = None
        self.policy = ''
        self.secret = ''
        self.fail = None
        self.listen_ports = ''
        self.other_addresses = []
        self.ipv4_routes = []
        self.nexthops = []

    def available(self, command):
        return False

    def run(self, *args, input=None):
        self.calls.append((args, input))
        if self.fail and args[:len(self.fail)] == self.fail:
            raise helper.Rejected('simulated command failure')
        if args == ('ip', '-j', '-d', 'link', 'show'):
            return json.dumps(self.links)
        if args == ('ip', '-j', '-N', 'nexthop', 'show'):
            return json.dumps(self.nexthops)
        if args[:5] == ('ip', '-j', '-N', '-6', 'route'):
            assert args[5:] == ('show', 'table', 'all')
            return json.dumps(self.routes)
        if args[:5] == ('ip', '-j', '-N', '-4', 'route'):
            assert args[5:] == ('show', 'table', 'all')
            return json.dumps(self.ipv4_routes)
        if args[:3] == ('ip', '-j', 'address'):
            return json.dumps([{'ifname': DEV, 'addr_info': self.addresses}])
        if args[:4] == ('ip', '-j', '-6', 'address'):
            return json.dumps([{'ifname': DEV, 'addr_info': self.addresses}] + self.other_addresses)
        if args == ('nft', '-j', 'list', 'tables'):
            return json.dumps({'nftables': [{'table': {'name': helper.TABLE, 'family': 'inet'}}] if self.table else []})
        if args[:4] == ('nft', '-j', 'list', 'table'):
            return json.dumps({'nftables': [{'table': {'comment': self.table}}]})
        if args == ('nft', '-f', '-'):
            self.policy = input
            self.table = input.split('comment "')[1].split('"')[0]
        if args == ('wg', 'show', 'all', 'listen-port'):
            return self.listen_ports
        if args == ('wg', 'pubkey'):
            return KEY
        if args[:2] == ('wg', 'syncconf'):
            path = Path(args[3])
            assert path.stat().st_mode & 0o777 == 0o600
            self.secret = path.read_text()
        if args[:3] == ('ip', 'link', 'add'):
            self.links = [{'ifname': args[4], 'ifalias': args[args.index('alias') + 1],
                           'linkinfo': {'info_kind': 'wireguard'}}]
        if args[:3] == ('ip', 'link', 'set'):
            if args[5] == 'alias':
                self.links[0]['ifalias'] = args[6]
            else:
                self.links[0]['operstate'] = args[5]
        if args[:3] == ('ip', 'link', 'delete'):
            self.links = []
            self.routes = [route for route in self.routes if route.get('dev') != args[4]]
            self.ipv4_routes = [route for route in self.ipv4_routes if route.get('dev') != args[4]]
            self.addresses = []
        if args[:4] == ('ip', '-6', 'address', 'del'):
            self.addresses = [a for a in self.addresses if a['local'] + '/' + str(a['prefixlen']) != args[4]]
        if args[:4] == ('ip', '-6', 'address', 'replace'):
            ip, prefix = args[4].split('/')
            self.addresses = [{'local': ip, 'prefixlen': int(prefix), 'scope': 'global'}]
            if 'noprefixroute' not in args:
                self.routes.append({'dst': args[4], 'dev': args[6], 'protocol': '2', 'metric': 256})
        if args[:4] == ('ip', '-6', 'route', 'replace'):
            assert self.links[0]['operstate'] == 'up'
            self.routes = [r for r in self.routes if r['dst'] != args[4]]
            self.routes.append({'dst': args[4], 'dev': args[6], 'protocol': '242', 'metric': 42760})
        if args[:4] == ('ip', '-6', 'route', 'del'):
            self.routes = [r for r in self.routes if r['dst'] != args[4]]
        return ''


class ReconcileTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.addCleanup(patch.stopall)
        patch.object(helper, 'STATE', Path(self.tmp.name)).start()
        self.runner = FakeRunner()
        self.saved = None
        self.ssh = ''
        self.host = helper.Host(self.runner, {'owner': 'test-owner', 'networks': {}}, self.save, self.write_ssh)

    def save(self, state):
        self.saved = copy.deepcopy(state)

    def write_ssh(self, value):
        self.ssh = value

    def apply(self, data=None):
        return self.host.apply(ID, helper.validate(data or desired(), ID))

    def test_apply_and_remove_are_idempotent(self):
        self.assertEqual(self.apply()['applied_device_ids'], ['34' * 32])
        self.apply()
        self.assertEqual(len([c for c, _ in self.runner.calls if c[:3] == ('ip', 'link', 'add')]), 1)
        self.assertEqual(len(self.runner.routes), 1)
        self.assertIn('AllowedIPs = fd00:1234:5678::2/128', self.runner.secret)
        self.assertEqual(list(Path(self.tmp.name).iterdir()), [])
        self.assertNotIn(KEY, json.dumps([args for args, _ in self.runner.calls]))
        self.assertNotIn('private_key', json.dumps(self.saved))
        self.host.remove(ID)
        self.host.remove(ID)
        self.assertEqual(self.runner.links, [])
        self.assertEqual(self.ssh, '')
        self.assertEqual(self.saved['networks'], {})

    def test_large_accepted_state_can_reapply_revoke_and_restore(self):
        configs = {}
        first = None
        for n in range(32):
            ident = ID if n == 0 else f'{n:010x}' + 'ef' * 11
            data = desired()
            data.update(network_id=ident, address=f'fd00:{n+1:x}:5678::1/128', listen_port=51820+n)
            data['peers'] = [dict(data['peers'][0], device_id=f'{i+256:064x}', name='a'*128,
                                 public_key=base64.b64encode((i+1).to_bytes(32, 'little')).decode(),
                                 address=f'fd00:{n+1:x}:5678::{i+2:x}/128') for i in range(256)]
            data = helper.validate(data, ident)
            config = {k: v for k, v in data.items() if k != 'private_key'}
            config.update(alias='tako-network:' + ident + ':' + '00'*16, managed_addresses=[data['address']], retiring_routes=[])
            candidate = {'owner': 'test-owner', 'networks': {**configs, ident: config}}
            if len((json.dumps(candidate) + '\n').encode()) > helper.MAX_BYTES:
                break
            configs[ident] = config
            if first is None:
                first = data
        state = {'owner': 'test-owner', 'networks': configs}
        size = len(helper.serialize_state(state).encode())
        self.assertGreater(size + len(json.dumps(configs[ID])), helper.MAX_BYTES)
        self.host = helper.Host(self.runner, state, self.save, self.write_ssh)
        self.apply(first)
        self.apply(first)
        reduced = copy.deepcopy(first)
        reduced['peers'].pop()
        self.apply(reduced)
        self.assertEqual(len(self.saved['networks'][ID]['peers']), 255)
        self.runner.links, self.runner.addresses, self.runner.routes, self.runner.table = [], [], [], None
        self.host = helper.Host(self.runner, copy.deepcopy(self.saved), self.save, self.write_ssh)
        self.apply(reduced)
        self.assertEqual(self.runner.links[0]['operstate'], 'up')

    def test_state_limit_counts_exact_persisted_bytes_including_newline(self):
        value = {'owner': 'test', 'networks': {}, 'label': 'é'}
        content = json.dumps(value) + '\n'
        with patch.object(helper, 'MAX_BYTES', len(content.encode())):
            self.assertEqual(helper.serialize_state(value), content)
        with patch.object(helper, 'MAX_BYTES', len(content.encode()) - 1):
            with self.assertRaisesRegex(helper.Rejected, 'host state too large'):
                helper.serialize_state(value)

    def test_rejected_oversized_state_does_not_mutate_memory_or_disk(self):
        self.apply()
        before = copy.deepcopy(self.host.state)
        data = desired()
        data['peers'][0]['name'] = 'a' * 128
        with patch.object(helper, 'MAX_BYTES', len(helper.serialize_state(before).encode())):
            with self.assertRaisesRegex(helper.Rejected, 'host state too large'):
                self.apply(data)
        self.assertEqual(self.host.state, before)
        self.assertEqual(self.saved, before)

    def test_foreign_interface_not_reused_or_removed(self):
        self.runner.links = [{'ifname': DEV, 'ifalias': 'admin', 'linkinfo': {'info_kind': 'wireguard'}}]
        for operation in (self.apply, lambda: self.host.remove(ID)):
            with self.assertRaisesRegex(helper.Rejected, 'foreign interface'):
                operation()
        self.assertEqual(len(self.runner.links), 1)
        self.assertIsNone(self.saved)

    def test_apply_and_remove_preserve_foreign_addresses_in_both_families(self):
        self.apply()
        for foreign in ({'local': 'fd99::123', 'prefixlen': 64, 'scope': 'global'},
                        {'local': '192.0.2.1', 'prefixlen': 32, 'scope': 'global'},
                        {'local': '169.254.1.1', 'prefixlen': 16, 'scope': 'link'}):
            with self.subTest(foreign=foreign):
                self.runner.addresses.append(foreign)
                for operation in (self.apply, lambda: self.host.remove(ID)):
                    before = copy.deepcopy(self.runner.links)
                    with self.assertRaisesRegex(helper.Rejected, 'foreign interface address'):
                        operation()
                    self.assertEqual(self.runner.links, before)
                    self.assertIn(foreign, self.runner.addresses)
                self.runner.addresses.remove(foreign)

    def test_apply_and_remove_preserve_foreign_routes_in_all_tables_and_families(self):
        self.apply()
        for family in ('ipv4_routes', 'routes'):
            for table in ('254', '255', '100'):
                foreign = {'dst': '192.0.2.0/24' if family == 'ipv4_routes' else 'fd99::/64',
                           'dev': DEV, 'protocol': '4', 'metric': 500, 'table': table}
                with self.subTest(family=family, table=table):
                    routes = getattr(self.runner, family)
                    routes.append(foreign)
                    for operation in (self.apply, lambda: self.host.remove(ID)):
                        with self.assertRaisesRegex(helper.Rejected, 'foreign route'):
                            operation()
                        self.assertEqual(self.runner.links[0]['operstate'], 'up')
                        self.assertIn(foreign, routes)
                    routes.remove(foreign)

    def test_managed_route_markers_do_not_authorize_arbitrary_destinations_or_tables(self):
        self.apply()
        for foreign in ({'dst': 'fd99::1/128', 'dev': DEV, 'protocol': '242', 'metric': 42760},
                        {'dst': desired()['peers'][0]['address'], 'dev': DEV, 'protocol': '242', 'metric': 42760, 'table': '100'},
                        {'dst': 'fd99::/64', 'nexthops': [{'dev': DEV}], 'protocol': '4'}):
            self.runner.routes.append(foreign)
            with self.assertRaisesRegex(helper.Rejected, 'foreign route'):
                self.host.remove(ID)
            self.assertEqual(self.runner.links[0]['operstate'], 'up')
            self.runner.routes.remove(foreign)
        self.runner.nexthops = [{'id': 77, 'dev': DEV}]
        with self.assertRaisesRegex(helper.Rejected, 'foreign nexthop'):
            self.host.remove(ID)

    def test_kernel_local_and_multicast_routes_are_owned_but_unrelated_routes_survive(self):
        self.apply()
        self.runner.routes.extend([
            {'dst': desired()['address'], 'dev': DEV, 'type': 'local', 'table': '255', 'protocol': '2'},
            {'dst': 'ff00::/8', 'dev': DEV, 'type': 'multicast', 'table': '255', 'protocol': '2'}])
        foreign = {'dst': 'fd99::/64', 'dev': 'eth0', 'protocol': '4', 'table': '100'}
        self.runner.routes.append(foreign)
        self.host.remove(ID)
        self.assertEqual(self.runner.links, [])
        self.assertIn(foreign, self.runner.routes)

    def test_retry_revocation_retains_ownership_of_routes_pending_removal(self):
        self.apply()
        data = desired()
        data['peers'] = []
        self.runner.fail = ('ip', '-6', 'route', 'del')
        with self.assertRaises(helper.Rejected):
            self.apply(data)
        self.assertEqual(self.saved['networks'][ID]['retiring_routes'], [desired()['peers'][0]['address']])
        self.runner.fail = None
        self.host = helper.Host(self.runner, copy.deepcopy(self.saved), self.save, self.write_ssh)
        self.apply(data)
        self.assertEqual(self.runner.routes, [])
        self.assertEqual(self.saved['networks'][ID]['retiring_routes'], [])

    def test_foreign_firewall_not_reused(self):
        self.runner.table = 'admin-firewall'
        with self.assertRaisesRegex(helper.Rejected, 'foreign firewall'):
            self.apply()
        self.assertEqual(self.runner.policy, '')
        self.assertIsNone(self.saved)

    def test_foreign_interface_address_not_reused(self):
        self.runner.other_addresses = [{'ifname': 'eth0', 'addr_info': [
            {'local': 'fd00:1234:5678::1', 'prefixlen': 64, 'scope': 'global'}]}]
        with self.assertRaisesRegex(helper.Rejected, 'address belongs to another interface'):
            self.apply()
        self.assertIsNone(self.saved)

    def test_kernel_link_local_route_is_preserved(self):
        self.apply()
        route = {'dst': 'fe80::/64', 'dev': DEV, 'protocol': '2', 'metric': 256}
        self.runner.routes.append(route)
        self.apply()
        self.assertIn(route, self.runner.routes)

    def test_foreign_route_not_replaced(self):
        self.runner.routes = [{'dst': 'fd00:1234:5678::2/128', 'dev': 'eth0'}]
        with self.assertRaisesRegex(helper.Rejected, 'foreign route'):
            self.apply()
        self.assertIsNone(self.saved)

    def test_revocation_removes_ssh_routes_and_wireguard_peer(self):
        self.apply()
        data = desired()
        data['peers'] = []
        self.apply(data)
        self.assertEqual(self.ssh, '')
        self.assertEqual(self.runner.routes, [])
        self.assertNotIn('[Peer]', self.runner.secret)
        self.assertNotIn('tcp dport 22 accept', self.runner.policy)

    def test_policy_separates_membership_ssh_and_server_gossip(self):
        data = desired()
        data['peers'][0].pop('ssh_user')
        data['peers'][0].pop('ssh_public_key')
        self.apply(data)
        self.assertEqual(self.ssh, '')
        self.assertNotIn('tcp dport 22 accept', self.runner.policy)
        self.assertNotIn('udp dport 8787 accept', self.runner.policy)
        self.assertNotIn('tcp dport 9850 accept', self.runner.policy)
        data['peers'][0]['kind'] = 'server'
        self.apply(data)
        self.assertIn('udp dport 8787 accept', self.runner.policy)
        self.assertIn('tcp dport 9850 accept', self.runner.policy)
        self.assertIn('udp dport 8787 drop', self.runner.policy)
        self.assertIn('tcp dport 9850 drop', self.runner.policy)
        self.assertIn(f'iifname "{DEV}" drop', self.runner.policy)
        self.apply()
        self.assertIn('from="fd00:1234:5678::2",no-agent-forwarding,no-X11-forwarding,no-port-forwarding', self.ssh)

    def test_mutation_failure_brings_owned_interface_down_and_retry_recovers(self):
        self.apply()
        self.runner.fail = ('wg', 'syncconf')
        with self.assertRaises(helper.Rejected):
            self.apply()
        self.assertEqual(self.runner.links[0]['operstate'], 'down')
        self.runner.fail = None
        self.apply()
        self.assertEqual(self.runner.links[0]['operstate'], 'up')

    def test_retry_recovers_address_change_after_command_failure(self):
        self.apply()
        changed = desired()
        changed['address'] = 'fd00:1234:5678::9/128'
        self.runner.fail = ('wg', 'syncconf')
        with self.assertRaises(helper.Rejected):
            self.apply(changed)
        self.runner.fail = None
        self.host = helper.Host(self.runner, copy.deepcopy(self.saved), self.save, self.write_ssh)
        self.apply(changed)
        self.assertEqual(self.runner.addresses[0]['local'], 'fd00:1234:5678::9')
        self.assertEqual(self.saved['networks'][ID]['managed_addresses'], [changed['address']])

    def test_process_death_after_creation_leaves_atomic_ownership_for_retry_or_remove(self):
        for action in ('apply', 'remove'):
            with self.subTest(action=action):
                self.runner = FakeRunner()
                self.host = helper.Host(self.runner, {'owner': 'test-owner', 'networks': {}}, self.save, self.write_ssh)
                run = self.runner.run
                def die_after_create(*args, **kwargs):
                    result = run(*args, **kwargs)
                    if args[:3] == ('ip', 'link', 'add'):
                        raise SystemExit('simulated process death')
                    return result
                with patch.object(self.runner, 'run', side_effect=die_after_create):
                    with self.assertRaises(SystemExit):
                        self.apply()
                self.assertEqual(self.runner.links[0]['ifalias'], self.saved['networks'][ID]['alias'])
                self.host = helper.Host(self.runner, copy.deepcopy(self.saved), self.save, self.write_ssh)
                if action == 'apply':
                    self.apply()
                    self.assertEqual(self.runner.links[0]['operstate'], 'up')
                else:
                    self.host.remove(ID)
                    self.assertEqual(self.runner.links, [])

    def test_interface_create_failure_leaves_no_untagged_object(self):
        self.runner.fail = ('ip', 'link', 'add')
        with self.assertRaises(helper.Rejected):
            self.apply()
        self.assertEqual(self.runner.links, [])
        self.runner.fail = None
        self.apply()
        self.assertEqual(self.runner.links[0]['operstate'], 'up')
        self.assertFalse(any(args[:6] == ('ip', 'link', 'set', 'dev', DEV, 'alias')
                             for args, _ in self.runner.calls))

    def test_reboot_recreates_only_owned_persistent_network(self):
        self.apply()
        self.runner.links = []
        self.runner.routes = []
        self.runner.addresses = []
        self.runner.table = None
        self.host = helper.Host(self.runner, copy.deepcopy(self.saved), self.save, self.write_ssh)
        self.apply()
        self.assertEqual(self.runner.links[0]['operstate'], 'up')
        self.assertEqual(self.runner.table, 'test-owner')

    def test_interface_prefix_collision_fails_before_mutation(self):
        self.apply()
        other = ID[:10] + 'ef' * 11
        value = desired()
        value['network_id'] = other
        with self.assertRaisesRegex(helper.Rejected, 'interface name collision'):
            self.host.apply(other, helper.validate(value, other))
        self.assertEqual(list(self.saved['networks']), [ID])

    def test_foreign_wireguard_listen_port_is_not_shared(self):
        self.runner.listen_ports = 'wg0\t51820\n'
        with self.assertRaisesRegex(helper.Rejected, 'listen port in use'):
            self.apply()
        self.assertIsNone(self.saved)

    def test_sshd_can_read_root_owned_public_keys_despite_private_umask(self):
        fd = os.open(self.tmp.name, os.O_RDONLY | os.O_DIRECTORY)
        previous = os.umask(0o077)
        try:
            helper.write_file(fd, 'authorized_keys', SSH_KEY, 0o644)
            self.assertEqual((Path(self.tmp.name) / 'authorized_keys').stat().st_mode & 0o777, 0o644)
        finally:
            os.umask(previous)
            os.close(fd)

    def test_active_host_firewalls_allow_transport_and_scoped_overlay(self):
        calls = []
        class Firewalls:
            def available(self, command):
                return True
            def run(self, *args):
                calls.append(args)
                if args == ('ufw', 'status'):
                    return 'Status: active\n'
                if args[0] == 'systemctl':
                    return 'active\n'
                return ''
        self.host.runner = Firewalls()
        self.host.state['networks'][ID] = helper.validate(desired(), ID)
        self.host.open_host_firewall(ID, 51820)
        self.assertIn(('ufw', 'allow', '51820/udp', 'comment', 'tako-network-' + ID), calls)
        self.assertIn(('firewall-cmd', '--add-port=51820/udp'), calls)
        self.assertIn(('firewall-cmd', '--permanent', '--add-port=51820/udp'), calls)
        self.assertIn(('ufw', 'allow', 'in', 'on', DEV, 'from', 'fd00:1234:5678::/48',
                       'to', 'fd00:1234:5678::1', 'comment', 'tako-network-' + ID), calls)
        self.assertTrue(any('destination address="fd00:1234:5678::1"' in arg
                            for call in calls for arg in call))
        self.assertFalse(any('flush' in arg or 'disable' in arg for call in calls for arg in call))

    def test_foreign_interface_with_managed_name_after_apply_is_not_stopped(self):
        self.apply()
        self.runner.links[0]['ifalias'] = 'foreign'
        with self.assertRaises(helper.Rejected):
            self.host.remove(ID)
        self.assertEqual(self.runner.links[0]['operstate'], 'up')


if __name__ == '__main__':
    unittest.main()

import base64
import copy
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
            ('listen_port', True), ('listen_port', 0), ('listen_port', 65536),
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
            ('endpoint', '10.0.0.1:80'), ('endpoint', '169.254.169.254:80'),
            ('endpoint', '[fd00::1]:80'), ('endpoint', '[::1]:80'),
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

    def available(self, command):
        return False

    def run(self, *args, input=None):
        self.calls.append((args, input))
        if self.fail and args[:len(self.fail)] == self.fail:
            raise helper.Rejected('simulated command failure')
        if args == ('ip', '-j', '-d', 'link', 'show'):
            return json.dumps(self.links)
        if args[:4] == ('ip', '-j', '-6', 'route'):
            return json.dumps(self.routes)
        if args[:4] == ('ip', '-j', '-6', 'address'):
            return json.dumps([{'addr_info': self.addresses}])
        if args == ('nft', '-j', 'list', 'tables'):
            return json.dumps({'nftables': [{'table': {'name': helper.TABLE, 'family': 'inet'}}] if self.table else []})
        if args[:4] == ('nft', '-j', 'list', 'table'):
            return json.dumps({'nftables': [{'table': {'comment': self.table}}]})
        if args == ('nft', '-f', '-'):
            self.policy = input
            self.table = input.split('comment "')[1].split('"')[0]
        if args == ('wg', 'pubkey'):
            return KEY
        if args[:2] == ('wg', 'syncconf'):
            path = Path(args[3])
            assert path.stat().st_mode & 0o777 == 0o600
            self.secret = path.read_text()
        if args[:3] == ('ip', 'link', 'add'):
            self.links = [{'ifname': args[4], 'linkinfo': {'info_kind': 'wireguard'}}]
        if args[:3] == ('ip', 'link', 'set'):
            if args[5] == 'alias':
                self.links[0]['ifalias'] = args[6]
            else:
                self.links[0]['operstate'] = args[5]
        if args[:3] == ('ip', 'link', 'delete'):
            self.links = []
            self.routes = []
            self.addresses = []
        if args[:4] == ('ip', '-6', 'address', 'replace'):
            ip, prefix = args[4].split('/')
            self.addresses = [{'local': ip, 'prefixlen': int(prefix), 'scope': 'global'}]
        if args[:4] == ('ip', '-6', 'route', 'replace'):
            self.routes = [r for r in self.routes if r['dst'] != args[4]]
            self.routes.append({'dst': args[4], 'dev': args[6], 'protocol': '186', 'metric': 42760})
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

    def test_foreign_interface_not_reused_or_removed(self):
        self.runner.links = [{'ifname': DEV, 'ifalias': 'admin', 'linkinfo': {'info_kind': 'wireguard'}}]
        for operation in (self.apply, lambda: self.host.remove(ID)):
            with self.assertRaisesRegex(helper.Rejected, 'foreign interface'):
                operation()
        self.assertEqual(len(self.runner.links), 1)
        self.assertIsNone(self.saved)

    def test_foreign_firewall_not_reused(self):
        self.runner.table = 'admin-firewall'
        with self.assertRaisesRegex(helper.Rejected, 'foreign firewall'):
            self.apply()
        self.assertEqual(self.runner.policy, '')
        self.assertIsNone(self.saved)

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
        data['peers'][0]['kind'] = 'server'
        self.apply(data)
        self.assertIn('udp dport 8787 accept', self.runner.policy)
        self.assertIn('udp dport 8787 drop', self.runner.policy)
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

    def test_foreign_interface_with_managed_name_after_apply_is_not_stopped(self):
        self.apply()
        self.runner.links[0]['ifalias'] = 'foreign'
        with self.assertRaises(helper.Rejected):
            self.host.remove(ID)
        self.assertEqual(self.runner.links[0]['operstate'], 'up')


if __name__ == '__main__':
    unittest.main()

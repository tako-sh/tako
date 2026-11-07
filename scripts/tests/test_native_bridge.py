import copy
import json
import unittest

from scripts.tests import test_network_helper as fixtures
from scripts.tests.test_network_policy import input_verdict

helper, ID, DEV = fixtures.helper, fixtures.ID, fixtures.DEV


class BridgeRunner(fixtures.FakeRunner):
    def __init__(self):
        super().__init__()
        self.networks = []
        self.bridge_addresses = []

    def available(self, command):
        return command == 'docker'

    def network(self):
        config = helper.bridge_config(ID)
        return {'Id': 'aa' * 32, 'Name': config['name'], 'Driver': 'bridge', 'Scope': 'local',
                'Internal': True, 'EnableIPv6': False, 'EnableIPv4': True, 'Ingress': False,
                'Labels': {'sh.tako.network': ID, 'sh.tako.owner': 'test-owner'},
                'Options': helper.bridge_options(ID), 'Containers': {},
                'IPAM': {'Driver': 'default', 'Options': {},
                         'Config': [{'Subnet': config['subnet'], 'Gateway': config['gateway']}]}}

    def create(self):
        config = helper.bridge_config(ID)
        self.networks.append(self.network())
        self.links.append({'ifname': config['interface'], 'linkinfo': {'info_kind': 'bridge'}})
        self.bridge_addresses = [{'ifname': config['interface'], 'addr_info': [
            {'local': config['gateway'], 'prefixlen': 24, 'scope': 'global'}]}]
        self.ipv4_routes.append({'dst': config['subnet'], 'dev': config['interface'], 'protocol': '2'})
        self.ipv4_routes.extend([
            {'dst': config['gateway'], 'dev': config['interface'], 'protocol': '2', 'table': '255', 'type': '2'},
            {'dst': '10.75.171.255', 'dev': config['interface'], 'protocol': '2', 'table': '255', 'type': '3'},
        ])

    def run(self, *args, input=None):
        if args == ('ip', '-j', 'address', 'show'):
            self.calls.append((args, input))
            return json.dumps([{'ifname': DEV, 'addr_info': self.addresses}] + self.bridge_addresses + self.other_addresses)
        if args[0] != 'docker':
            return super().run(*args, input=input)
        self.calls.append((args, input))
        assert args[:6] == ('docker', '--host', 'unix:///var/run/docker.sock', '--config',
                            str(helper.STATE / 'docker'), 'network')
        if self.fail and args[:len(self.fail)] == self.fail:
            raise helper.Rejected('simulated Docker failure')
        command = args[6:]
        if command == ('ls', '--quiet', '--no-trunc'):
            return '\n'.join(n['Id'] for n in self.networks)
        if command[0] == 'inspect':
            assert set(command[1:]) == {n['Id'] for n in self.networks}
            return json.dumps(self.networks)
        if command[0] == 'create':
            assert 'iifname "' + helper.bridge_config(ID)['interface'] + '" drop' in self.policy
            self.create()
            return self.networks[-1]['Id']
        if command[0] == 'rm':
            assert command == ('rm', self.network()['Id'])
            self.networks = []
            dev = helper.bridge_config(ID)['interface']
            self.links = [link for link in self.links if link['ifname'] != dev]
            self.ipv4_routes = [route for route in self.ipv4_routes if route.get('dev') != dev]
            self.bridge_addresses = []
            return command[1]
        raise AssertionError(args)


class NativeBridgeTests(unittest.TestCase):
    save = fixtures.ReconcileTests.save
    write_ssh = fixtures.ReconcileTests.write_ssh
    apply = fixtures.ReconcileTests.apply

    def setUp(self):
        fixtures.ReconcileTests.setUp(self)
        self.runner = BridgeRunner()
        self.host.runner = self.runner
        self.apply()

    def docker_mutations(self):
        return [args for args, _ in self.runner.calls if args[0] == 'docker' and args[6] in ('create', 'rm')]

    def test_native_only_startup_does_not_require_docker(self):
        self.assertFalse(any(args[0] == 'docker' for args, _ in self.runner.calls))
        self.host.remove(ID)
        self.assertFalse(any(args[0] == 'docker' for args, _ in self.runner.calls))

    def test_bridge_requires_configured_wireguard(self):
        self.host.state['networks'] = {}
        with self.assertRaises(helper.Rejected):
            self.host.bridge(ID)
        self.assertEqual(self.docker_mutations(), [])

    def test_bridge_is_lazy_owned_internal_idempotent_and_removed_when_empty(self):
        config = self.host.bridge(ID)
        self.assertEqual(config, {'name': 'tako-svc-' + ID[:12], 'interface': 'tks' + ID[:12],
                                 'subnet': '10.75.171.0/24', 'gateway': '10.75.171.1'})
        self.host.bridge(ID)
        self.apply()
        self.assertTrue(self.saved['networks'][ID]['compose_bridge'])
        commands = self.docker_mutations()
        self.assertEqual(len(commands), 1)
        create = commands[0]
        self.assertIn('--internal', create)
        self.assertIn('com.docker.network.bridge.enable_icc=false', create)
        self.assertIn('com.docker.network.bridge.enable_ip_masquerade=false', create)
        self.assertIn('sh.tako.network=' + ID, create)
        self.assertIn('sh.tako.owner=test-owner', create)
        self.host.remove(ID)
        self.assertEqual(self.runner.networks, [])
        self.assertEqual(self.saved['networks'], {})
        self.assertEqual(self.docker_mutations()[-1][-2:], ('rm', 'aa' * 32))

    def test_unowned_name_prefix_or_modified_ipam_and_settings_are_rejected(self):
        changes = [ {'Labels': {}}, {'Labels': {'sh.tako.network': ID[:12] + '00' * 10,
                    'sh.tako.owner': 'test-owner'}}, {'Internal': False}, {'EnableIPv6': True},
                    {'Options': {}}, {'Driver': 'overlay'}, {'Scope': 'swarm'},
                    {'IPAM': {'Driver': 'default', 'Config': [{'Subnet': '10.0.1.0/24', 'Gateway': '10.0.1.1'}]}} ]
        self.runner.create()
        original = self.runner.network()
        for change in changes:
            with self.subTest(change=change):
                self.runner.networks = [{**copy.deepcopy(original), **change}]
                with self.assertRaises(helper.Rejected):
                    self.host.bridge(ID)
                self.assertNotIn('compose_bridge', self.saved['networks'][ID])
                self.assertEqual(self.docker_mutations(), [])

    def test_docker_subnet_and_host_address_route_or_interface_collisions_are_rejected(self):
        config = helper.bridge_config(ID)
        foreign = self.runner.network()
        foreign.update(Name='foreign', Labels={})
        cases = [ ('networks', [foreign]),
                  ('links', self.runner.links + [{'ifname': config['interface'], 'linkinfo': {'info_kind': 'bridge'}}]),
                  ('other_addresses', [{'ifname': 'eth7', 'addr_info': [{'local': config['gateway'], 'prefixlen': 24}]}]),
                  ('ipv4_routes', [{'dst': '10.0.0.0/8', 'dev': 'eth7'}]),
                  ('ipv4_routes', [{'dst': 'default', 'dev': config['interface']}]) ]
        for field, value in cases:
            with self.subTest(field=field, value=value):
                original = getattr(self.runner, field)
                setattr(self.runner, field, value)
                try:
                    with self.assertRaises(helper.Rejected):
                        self.host.bridge(ID)
                    self.assertEqual(self.docker_mutations(), [])
                    self.assertNotIn('compose_bridge', self.saved['networks'][ID])
                finally:
                    setattr(self.runner, field, original)

    def test_owned_bridge_requires_its_actual_exact_gateway(self):
        self.runner.create()
        for addresses in ([], [{'local': '10.75.171.2', 'prefixlen': 24, 'scope': 'global'}]):
            with self.subTest(addresses=addresses):
                self.runner.bridge_addresses[0]['addr_info'] = addresses
                with self.assertRaises(helper.Rejected):
                    self.host.bridge(ID)
                self.assertEqual(self.docker_mutations(), [])

    def test_creation_failure_leaves_firewall_guard_and_retryable_state(self):
        prefix = ('docker', '--host', 'unix:///var/run/docker.sock', '--config', str(helper.STATE / 'docker'), 'network', 'create')
        self.runner.fail = prefix
        with self.assertRaises(helper.Rejected):
            self.host.bridge(ID)
        self.assertTrue(self.saved['networks'][ID]['compose_bridge'])
        self.assertIn('iifname "' + helper.bridge_config(ID)['interface'] + '" drop', self.runner.policy)
        self.runner.fail = None
        self.host.bridge(ID)
        self.assertEqual(len(self.runner.networks), 1)

    def test_remove_refuses_active_containers_or_foreign_changes_before_wireguard_mutation(self):
        self.host.bridge(ID)
        for change in ({'Containers': {'container': {}}}, {'Options': {}}, {'Labels': {}}):
            with self.subTest(change=change):
                self.runner.networks = [{**self.runner.network(), **change}]
                before = len(self.runner.calls)
                with self.assertRaises(helper.Rejected):
                    self.host.remove(ID)
                self.assertFalse(any(args[:3] == ('ip', 'link', 'set') for args, _ in self.runner.calls[before:]))
                self.assertFalse(any(args[-2:] == ('rm', 'aa' * 32) for args, _ in self.runner.calls[before:]))

    def test_bridge_gateway_only_admits_actual_interface_subnet_and_http_dns(self):
        self.host.bridge(ID)
        bridge = helper.bridge_config(ID)
        packet = {'iifname': bridge['interface'], 'ip saddr': '10.75.171.2',
                  'ip daddr': bridge['gateway'], 'meta l4proto': 'tcp', 'dport': 80}
        for protocol, port in (('tcp', 80), ('tcp', 53), ('udp', 53)):
            self.assertEqual(input_verdict(self.runner.policy, {**packet, 'meta l4proto': protocol, 'dport': port}), 'accept')
        for change in ({'iifname': 'eth0'}, {'iifname': 'lo'}, {'ip saddr': '192.168.1.2'},
                       {'ip daddr': '10.75.171.9'}, {'dport': 443}, {'dport': 9844}, {'dport': 22},
                       {'meta l4proto': 'udp', 'dport': 80}):
            self.assertEqual(input_verdict(self.runner.policy, {**packet, **change}), 'drop')
        forward = self.runner.policy.split('chain forward')[1]
        for direction in ('iifname', 'oifname'):
            self.assertIn(f'{direction} "{bridge["interface"]}" drop', forward)
        self.assertNotIn(' accept\n', forward)


if __name__ == '__main__':
    unittest.main()

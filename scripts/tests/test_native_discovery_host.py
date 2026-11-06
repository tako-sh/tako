import copy
import unittest

from scripts.tests import test_network_helper as fixtures

helper, FakeRunner, ID, DEV = fixtures.helper, fixtures.FakeRunner, fixtures.ID, fixtures.DEV


class ResolverRunner(FakeRunner):
    def __init__(self):
        super().__init__()
        self.resolver = {'dns': ['9.9.9.9'], 'domain': ['~corp.example'], 'default-route': ['yes']}
        self.active = 'active'

    def available(self, command):
        return command == 'resolvectl'

    def run(self, *args, input=None):
        if self.fail and args[:len(self.fail)] == self.fail:
            raise helper.Rejected('simulated resolver failure')
        if args[:3] == ('systemctl', 'show', 'systemd-resolved.service'):
            self.calls.append((args, input))
            return self.active + '\n'
        if args[0] == 'resolvectl':
            self.calls.append((args, input))
            assert args[2] == DEV
            if len(args) == 3:
                return f'Link 10 ({DEV}): ' + ' '.join(self.resolver[args[1]]) + '\n'
            self.resolver[args[1]] = [value for value in args[3:] if value]
            return ''
        return super().run(*args, input=input)


class ResolverTests(unittest.TestCase):
    setUp = fixtures.ReconcileTests.setUp
    save = fixtures.ReconcileTests.save
    write_ssh = fixtures.ReconcileTests.write_ssh
    apply = fixtures.ReconcileTests.apply

    def test_resolver_only_routes_native_zone_and_restores_original_link_settings(self):
        self.runner = ResolverRunner()
        self.host.runner = self.runner
        original = copy.deepcopy(self.runner.resolver)
        self.assertTrue(self.apply()['dns_supported'])
        self.assertEqual(self.runner.resolver, {'dns': ['fd00:1234:5678::1'],
            'domain': ['~' + ID + '.tako.internal'], 'default-route': ['no']})
        self.assertTrue(self.apply()['dns_supported'])
        self.assertEqual(self.saved['networks'][ID]['dns_settings']['original'], original)
        self.host.remove(ID)
        self.assertEqual(self.runner.resolver, original)
        self.assertFalse(any(args[0] == 'resolvectl' and args[1] == 'revert' for args, _ in self.runner.calls))

    def test_missing_or_inactive_resolver_reports_without_disabling_wireguard(self):
        result = self.apply()
        self.assertFalse(result['dns_supported'])
        self.assertIn('systemd-resolved', result['dns_error'])
        self.assertEqual(self.runner.links[0]['operstate'], 'up')
        self.runner = ResolverRunner()
        self.runner.active = 'inactive'
        self.host.runner = self.runner
        result = self.apply()
        self.assertFalse(result['dns_supported'])
        self.assertEqual(self.runner.links[0]['operstate'], 'up')
        self.assertFalse(any(args[0] == 'resolvectl' for args, _ in self.runner.calls))

    def test_teardown_does_not_overwrite_administrator_dns_changes(self):
        self.runner = ResolverRunner()
        self.host.runner = self.runner
        self.apply()
        self.runner.resolver['domain'] = ['~administrator.example']
        result = self.apply()
        self.assertFalse(result['dns_supported'])
        self.assertIn('changed outside Tako', result['dns_error'])
        self.host.remove(ID)
        self.assertEqual(self.runner.resolver['domain'], ['~administrator.example'])
        self.assertEqual(self.runner.resolver['dns'], ['9.9.9.9'])

    def test_partial_resolver_failure_restores_only_settings_it_changed(self):
        self.runner = ResolverRunner()
        self.host.runner = self.runner
        original = copy.deepcopy(self.runner.resolver)
        self.runner.fail = ('resolvectl', 'domain', DEV, '~' + ID + '.tako.internal')
        result = self.apply()
        self.assertFalse(result['dns_supported'])
        self.assertEqual(self.runner.resolver, original)
        self.assertEqual(self.runner.links[0]['operstate'], 'up')

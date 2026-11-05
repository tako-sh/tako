import shlex
import unittest

from scripts.tests.test_network_helper import helper, desired, ID, DEV


def input_verdict(policy, packet):
    """Evaluate the generated input chain's selectors; no claim of nft/kernel execution."""
    rules = policy.split('chain input { type filter hook input priority -10; policy accept;\n')[1].split('\n}\n')[0]
    for rule in rules.splitlines():
        tokens = shlex.split(rule)
        matches = True
        while len(tokens) > 1:
            field = tokens.pop(0)
            if field in ('ip6', 'ct', 'meta'):
                field += ' ' + tokens.pop(0)
            if field in ('tcp', 'udp'):
                matches &= packet['meta l4proto'] == field
                field = tokens.pop(0)
            inverse = tokens[0] == '!='
            if inverse:
                tokens.pop(0)
            if tokens[0] == '{':
                tokens.pop(0)
                values = []
                while tokens[0] != '}':
                    values.append(tokens.pop(0).rstrip(','))
                tokens.pop(0)
            else:
                values = [tokens.pop(0)]
            matches &= (str(packet.get(field)) in values) != inverse
        if matches:
            return tokens[0]
    return 'accept'


class NetworkPolicyTests(unittest.TestCase):
    def policy(self, kind='laptop', ssh=True, peers=True):
        config = desired()
        config['peers'][0]['kind'] = kind
        if not ssh:
            del config['peers'][0]['ssh_user']
            del config['peers'][0]['ssh_public_key']
        if not peers:
            config['peers'] = []
        return helper.firewall({'owner': 'test', 'networks': {ID: config}})

    def packet(self, **changes):
        return {'iifname': DEV, 'ip6 saddr': 'fd00:1234:5678::2',
                'ip6 daddr': 'fd00:1234:5678::1', 'meta l4proto': 'tcp',
                'sport': 9844, 'dport': 40000, 'ct state': 'established',
                'ct direction': 'reply', **changes}

    def test_own_wg_loopback_requires_exact_source_destination_and_service_port(self):
        policy = self.policy(peers=False)
        packet = self.packet(**{'iifname':'lo', 'ip6 saddr':'fd00:1234:5678::1',
            'ct state':'new', 'ct direction':'original', 'dport':80})
        self.assertEqual(input_verdict(policy, packet), 'accept')
        for change in ({'ip6 saddr':'::1'}, {'ip6 saddr':'fd00:1234:5678::2'},
                       {'dport':22}, {'iifname':'eth0'}):
            self.assertEqual(input_verdict(policy, {**packet, **change}), 'drop')
        packet.update({'ct state':'established', 'ct direction':'reply', 'dport':40000})
        self.assertEqual(input_verdict(policy, packet), 'accept')

    def test_tcp_replies_to_host_initiated_connections_work(self):
        for port in (22, 53, 80, 443, 9844):
            self.assertEqual(input_verdict(self.policy(ssh=False), self.packet(sport=port)), 'accept')

    def test_reply_exception_requires_current_peer_local_destination_and_reply_direction(self):
        for changes in ({'ip6 saddr': 'fd00:1234:5678::99'},
                        {'ip6 daddr': 'fd00:1234:5678::99'}, {'ct direction': 'original'},
                        {'ct state': 'new'}, {'iifname': 'eth0'}):
            with self.subTest(changes=changes):
                self.assertEqual(input_verdict(self.policy(), self.packet(**changes)), 'drop')
        self.assertEqual(input_verdict(self.policy(peers=False), self.packet()), 'drop')

    def test_established_inbound_ssh_does_not_survive_permission_removal(self):
        packet = self.packet(sport=40000, dport=22, **{'ct direction': 'original'})
        self.assertEqual(input_verdict(self.policy(), packet), 'accept')
        self.assertEqual(input_verdict(self.policy(ssh=False), packet), 'drop')
        self.assertEqual(input_verdict(self.policy(peers=False), packet), 'drop')

    def test_private_dns_tcp_udp_queries_and_replies(self):
        for protocol in ('tcp', 'udp'):
            query = self.packet(sport=40000, dport=53, **{'meta l4proto': protocol,
                                'ct state': 'new', 'ct direction': 'original'})
            reply = self.packet(sport=53, **{'meta l4proto': protocol})
            self.assertEqual(input_verdict(self.policy(), query), 'accept')
            self.assertEqual(input_verdict(self.policy(), reply), 'accept')
            self.assertEqual(input_verdict(self.policy(peers=False), query), 'drop')
            self.assertEqual(input_verdict(self.policy(peers=False), reply), 'drop')

    def test_gossip_and_its_replies_require_server_membership(self):
        for packet in (self.packet(dport=8787, **{'meta l4proto': 'udp'}),
                       self.packet(sport=8787, **{'meta l4proto': 'udp'})):
            self.assertEqual(input_verdict(self.policy(kind='server'), packet), 'accept')
            self.assertEqual(input_verdict(self.policy(kind='laptop'), packet), 'drop')
            self.assertEqual(input_verdict(self.policy(peers=False), packet), 'drop')


if __name__ == '__main__':
    unittest.main()

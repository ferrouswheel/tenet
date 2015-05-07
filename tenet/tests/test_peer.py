import time
import unittest
import codecs
from unittest import TestCase

from Crypto.PublicKey import RSA

from tenet.message import Message, MessageSerializer, MessageTypes, DictTransport
from tenet.peer import Peer


class TestPeer(TestCase):

    def setUp(self):
        self.peer = Peer('blah@example.com')
        self.peer.add_friend('blah2@example.com', RSA.generate(1024))

        self.transport = DictTransport()

    def test_handle_message(self):
        pass

    def test_store_message(self):
        pass

    def test_send(self):
        pass

    def test_online_friends(self):

        self.assertEqual(self.peer.online_friends, [])

        self.peer.friends['blah2@example.com'].online = True
        self.assertEqual(self.peer.online_friends, [self.peer.friends['blah2@example.com']])

    def test_str(self):
        self.assertEqual(str(self.peer), 'Peer blah@example.com')

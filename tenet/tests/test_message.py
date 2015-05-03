import time
import unittest
import codecs
from unittest import TestCase

from tenet.message import Message, MessageSerializer, MessageTypes
from tenet.peer import Peer


class TestMessage(TestCase):

    def setUp(self):
        pass

    def test_as_bytes(self):
        m = Message("blah@blah.com", ["blah@blah.com"], MessageTypes.MESSAGE,
                testdata="hello joe")

        self.assertEqual(Message.from_bytes(m.as_bytes()).as_dict(),
                m.as_dict(),
                "Byte encoding of message is correct")

    def test_as_dict(self):
        m = Message("blah@blah.com", ["blah@blah.com"], MessageTypes.MESSAGE,
                testdata="hello joe")

        self.assertEqual(m.as_dict(),
                {'author': 'blah@blah.com',
                  'data': {'testdata': 'hello joe'},
                  'recipients': ['blah@blah.com'],
                  'type': 3},
                "Dict representation of message is correct")

    def test_from_bytes(self):
        m_bytes = bytes('{"type": 3, "recipients": ["blah@blah.com"], "author": "blah@blah.com", "data": {"testdata": "hello joe"}}', 'utf-8')

        m = Message.from_bytes(m_bytes)

        self.assertEqual(m.as_dict(),
                {'author': 'blah@blah.com',
                  'data': {'testdata': 'hello joe'},
                  'recipients': ['blah@blah.com'],
                  'type': 3},
                "Loading from bytes, message is correct")

    def test_from_dict(self):
        msg_dict = {
            "author": '',
            "recipients": [''],
            "type": 1,
            "data": {}
        }
        m = Message.from_dict(msg_dict)

        self.assertEqual(m.as_dict(),
                msg_dict,
                "Loading from dict, message is correct")


class TestMessageSerializer(TestCase):

    def setUp(self):
        pass

    def test_encrypt_cycle(self):
        m = Message("blah@blah.com", ["blah@blah.com"], MessageTypes.MESSAGE,
                testdata="hello joe")

        p = Peer('blah@blah.com')
        ms = MessageSerializer()
        blobs = ms.encrypt(m, {'blah@blah.com' : p.key} )

        self.assertEqual(len(blobs), 1, "only one blob")

        (recipients, blob) = blobs[0]

        self.assertEqual(recipients, ['blah@blah.com'], "blob is for recipient")
        self.assertEqual(ms._num_keys_to_check(blob, p), (1, 11), "only one bridge key to check")

        d_m = ms.decrypt(blob, p)

        self.assertEqual(d_m.as_dict(), m.as_dict(),
                "Message is the same after encryption and decryption")

    def test_encrypt_cycle_multi_recipients(self):
        m = Message("blah@blah.com",
                ["blah0@blah.com", "blah1@blah.com", "blah2@blah.com"],
                MessageTypes.MESSAGE,
                testdata="hello joe")

        peers = []
        keys = {}
        for p in range(0, 3):
            address = 'blah%d@blah.com' % p
            pp = Peer(address)
            peers.append(pp)
            keys[address] = pp.key

        ms = MessageSerializer()
        blobs = ms.encrypt(m, keys)

        self.assertEqual(len(blobs), 1, "only one blob")

        (recipients, blob) = blobs[0]

        for p in peers:
            self.assertTrue(p.address in recipients, "blob is for recipient")
            d_m = ms.decrypt(blob, p)

            self.assertEqual(ms._num_keys_to_check(blob, p), (3, 11))

            self.assertEqual(d_m.as_dict(), m.as_dict(),
                "Message is the same after encryption and decryption")

    def test_encrypt_cycle_wrong_recipient(self):
        m = Message("blah@blah.com",
                ["blah0@blah.com", "blah1@blah.com", "blah2@blah.com"],
                MessageTypes.MESSAGE,
                testdata="hello joe")

        peers = []
        keys = {}
        for p in range(0, 3):
            address = 'blah%d@blah.com' % p
            pp = Peer(address)
            peers.append(pp)
            keys[address] = pp.key

        ms = MessageSerializer()
        blobs = ms.encrypt(m, keys)

        self.assertEqual(len(blobs), 1, "only one blob")

        (recipients, blob) = blobs[0]

        pp = Peer('someoneelse@blah.com')
        self.assertFalse(pp.address in recipients, "blob is not for recipient")

        self.assertEqual(ms._num_keys_to_check(blob, pp), (0, None))

        d_m = ms.decrypt(blob, pp)

        self.assertTrue(d_m is None)

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

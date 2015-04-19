import hashlib
import json
import codecs
import tenet.settings as settings

from bitarray import bitarray

from Crypto.Cipher import PKCS1_v1_5
from Crypto.Hash import SHA
from Crypto.Cipher import Blowfish
from Crypto import Random

from struct import pack

from tenet.hash import bloom_hash

_msg_count = None
def msg_count():
    global _msg_count
    if _msg_count is None:
        _msg_count = 0
    else:
        _msg_count += 1
    return _msg_count


class MessageTypes(object):
    # A status update to be shared with everyone
    BROADCAST = msg_count()
    # A status update to be shared with everyone anonymously
    BROADCAST_ANONYMOUS = msg_count()
    # A status update like a facebook post to friends
    SHARE = msg_count()
    # A chat room type interface
    MESSAGE = msg_count()
    # Comment on another message, though may only be supported for certain types
    COMMENT = msg_count()

    # Ask to peer and share keys
    PEER_REQUEST = msg_count() 
    # Accept peer request
    PEER_ACCEPT = msg_count()
    # Tell peer we no longer want updates sent, may be due to deletion
    # or just not wanting to be sent info.
    PEER_UNSUBSCRIBE = msg_count()

    # Let a peer know another peer is reputable
    GOSSIP_GOOD = msg_count()
    # Let a peer know another peer is behaving badly
    GOSSIP_BAD = msg_count()


class Message(object):
    """ The abstract message class for sending updates to peers.

    """

    def __init__(self, author, recipients, msg_type, **data):
        self.author = author
        self.recipients = recipients
        self.message_type = msg_type
        self.data = data

    def as_dict(self):
        return {
            "author": self.author.address,
            "recipients": [r.address for r in self.recipients],
            "type": self.message_type,
            "data": self.data
        }

    @classmethod
    def from_dict(cls, d):
        return Message(d['author'], d['recipients'], d['type'], **d['data'])

    def __hash__(self):
        h = hashlib.sha256()
        for k in self.data:
            h.update(k.encode('utf-8'))
            h.update(self.data[k].encode('utf-8'))
        return h


class MessageSerializer(object):
    """
    Takes a message and outputs bytes representing the
    message as an array of:

    [ (recipients1, message1), (recipients2, message2).. ]

    This involves sending message1 to each of the recipients in recipient1.

    Note, even though the content of message1 and message2 may be the same,
    they are different blobs of data because they can only be decrypted by
    their associated recipients (and the bloom header will customised for the
    recipients associated with that blob)

    Note, this doesn't mean each recipient should be sent to directly. That
    routing can be handled by MessageRouter.

    The message structure looks like:

    [ bloom header ][ C_key1 ][ C_key2 ]…[ C_keyN ][ shared encrypted blob]
    """

    def encrypt(self, msg):
        from tenet.utils import chunks

        blobs = []
        for recipients in chunks(msg.recipients, settings.MAX_RECIPIENTS_PER_MESSAGE):
            blobs.append((recipients, self._encrypt_for_recipients(recipients, msg)))

        return blobs

    def _encrypt_for_recipients(self, recipients, msg):
        filter_size = settings.MSG_BLOOM_FILTER_SIZE 
        bloom_header = bitarray(filter_size)
        bloom_header.setall(False)
        bridge_keys = []

        message = json.dumps(msg.as_dict()).encode('utf-8')

        msg_key = Random.new().read(56)
        print("Bridge key is", codecs.encode(msg_key, 'hex'))

        bs = Blowfish.block_size
        iv = Random.new().read(bs)
        print("iv is ", codecs.encode(iv, 'hex'))

        cipher = Blowfish.new(msg_key, Blowfish.MODE_CBC, iv)
        plen = bs - divmod(len(message),bs)[1]
        padding = [plen]*plen
        padding = pack('b'*plen, *padding)
        
        ciphertext = iv + cipher.encrypt(message + padding)
        print(codecs.encode(ciphertext, 'hex'))

        for recipient in recipients:
            # TODO don't use recipients for the bloom filter, use their
            # public keys
            bloom_header |= bloom_hash(recipient.address, filter_size)

            h = SHA.new(msg_key)
            cipher = PKCS1_v1_5.new(recipient.key)
            bk = cipher.encrypt(msg_key+h.digest())
            print(bk)
            print(len(bk))
            bridge_keys.append(bk)

        
        return (bloom_header.tobytes() +
                bytes([len(recipients)]) + b''.join(bridge_keys) +
                ciphertext)

    def decrypt(self, blob, peer):

        # To decrypt, first slice off the bloom_header, and check if there's a chance
        # one of the bridge_keys can be decrypted.
        # If so, then check each...
        filter_size = settings.MSG_BLOOM_FILTER_SIZE 

        bloom_end_index = int(filter_size/8)
        bloom_header = bitarray()
        bloom_header.frombytes(blob[:bloom_end_index])

        if bloom_hash(peer.address, filter_size):
            pass

        num_bridge_keys = int.from_bytes(blob[bloom_end_index:bloom_end_index+1],
                byteorder='little')
        current_index = bloom_end_index+1

        bridge_key = None
        for i in range(0,num_bridge_keys):
            ciphertext = blob[current_index:current_index+128]

            dsize = SHA.digest_size
            sentinel = Random.new().read(15+dsize) # Let's assume that average data length is 15

            cipher = PKCS1_v1_5.new(peer.key)
            bridge_key = cipher.decrypt(ciphertext, sentinel)

            digest = SHA.new(bridge_key[:-dsize]).digest()
            if digest==bridge_key[-dsize:]: # Note how we DO NOT look for the sentinel
                print("Decryption was correct.")
                bridge_key = bridge_key[:-dsize]
                current_index += 128
                break
            else:
                bridge_key = None
                print("Decryption was not correct, trying next bridge key.")
            current_index += 128

        if not bridge_key:
            return None

        print("Bridge key was", codecs.encode(bridge_key, 'hex'))

        iv = blob[current_index:(current_index + Blowfish.block_size)]
        print("iv is ", codecs.encode(iv, 'hex'))

        cipher = Blowfish.new(bridge_key, Blowfish.MODE_CBC, iv)
        print("length of blowfish ciphertext ", len(blob[current_index+Blowfish.block_size:]))
        message_text = cipher.decrypt(blob[current_index+Blowfish.block_size:])
        num_padding = int.from_bytes([message_text[-1]], byteorder='little')
        message_text = message_text[:-1 * num_padding]
        print(message_text)
        msg_dict = json.loads(codecs.decode(message_text, 'utf-8'))
        msg = Message.from_dict(msg_dict)

        return msg

class MessageRouter(object):
    """
    Given a recipient and a message, and the fact that the recipient might
    not currently be online, use the overlap in social contacts and their
    reliability to work out the best destination to be sent the message on their
    behalf.
    """

    def route(self, destination, message):
        """ Don't get fancy yet """
        return destination


class Transport(object):
    """
    A transport knows how to send a message based on the destination address.
    Note, the destination address might not be the person that can read the
    message.
    """

    @property
    def buffer_length(self):
        """ Return the number of messages the transport is willing to cache
        for a user
        """
        return 0

    def send_to(self, dest_address, message):
        """ Send a message to dest_address.

        Will return true if sent, false if buffered, and raise an exception
        if there was a problem.
        """
        pass


class InvalidAddress(Exception): pass


class DictTransport(Transport):
    """
    A transport for testing. Just associates a key (recipient) with a
    TenetPeer who will be directly sent any messages.
    """

    def __init__(self):
        self.peers = {}

    def send_to(self, dest, message):
        p = self.peers.get(dest.address)
        if p is None:
            raise InvalidAddress(dest.address)
        p.handle_message(message)
        return True

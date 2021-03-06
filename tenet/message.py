import json
import codecs
import logging

import tenet.settings as settings

from bitarray import bitarray

from Crypto.Cipher import PKCS1_v1_5, PKCS1_OAEP
from Crypto.Hash import SHA
from Crypto.Cipher import Blowfish
from Crypto import Random

from struct import pack

from tenet.hash import bloom_hash
from tenet.utils import chunks

log = logging.getLogger(__name__)


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

    # indicate we've come online and ask for recent messages
    PEER_ONLINE = msg_count() 
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
        self.blob_digest = None

    def as_dict(self):
        return {
            "author": self.author,
            "recipients": self.recipients,
            "type": self.message_type,
            "data": self.data
        }

    def as_bytes(self):
        return json.dumps(self.as_dict()).encode('utf-8')

    @classmethod
    def from_bytes(cls, message_bytes):
        msg_dict = json.loads(codecs.decode(message_bytes, 'utf-8'))
        return Message.from_dict(msg_dict)

    @classmethod
    def from_dict(cls, d):
        return Message(d['author'], d['recipients'], d['type'], **d['data'])


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

    def __init__(self):
        self.public_keys = {}

    def encrypt(self, msg, public_keys):
        blobs = []
        total_size = 0
        blob_count = 0
        self.public_keys.update(public_keys)
        orig_size = len(msg.as_bytes())
        for recipients in chunks(msg.recipients, settings.MAX_RECIPIENTS_PER_MESSAGE):
            blob = self._encrypt_for_recipients(recipients, msg)
            blob_count += 1
            total_size += len(blob)
            blobs.append((recipients, blob))

        log.debug("Total size of encrypted message is {} (orig: {} bytes, +{:.2f}%), across {} blobs.".format(
            total_size, orig_size, ((total_size - orig_size) * 100.0)/orig_size,
            blob_count
            ))

        return blobs

    def _key_for(self, address, private=False):
        return self.public_keys[address]

    def _encrypt_for_recipients(self, recipients, msg):

        # initialise the bloom filter
        filter_size = settings.MSG_BLOOM_FILTER_SIZE 
        bloom_header = bitarray(filter_size)
        bloom_header.setall(False)

        # keep a list of bridge_keys, each is the encrypted bridge key for
        # a particular recipient
        bridge_keys = []

        message = msg.as_bytes()

        # This is the secret inside each bridge key ciphertext
        msg_key = Random.new().read(56)
        log.debug("Bridge key is " + str(codecs.encode(msg_key, 'hex')))

        # Encrypt the message with the secret
        bs = Blowfish.block_size
        iv = Random.new().read(bs)

        cipher = Blowfish.new(msg_key, Blowfish.MODE_CBC, iv)
        plen = bs - divmod(len(message),bs)[1]
        padding = [plen]*plen
        padding = pack('b'*plen, *padding)
        
        ciphertext = iv + cipher.encrypt(message + padding)
        #log.debug(codecs.encode(ciphertext, 'hex'))

        # Now encrypt the bridge key using each recipients public key
        for recipient in recipients:
            # TODO don't use recipients email for the bloom filter, use their
            # public keys
            bloom_header |= bloom_hash(recipient, filter_size)

            #h = SHA.new(msg_key)
            cipher = PKCS1_OAEP.new(self._key_for(recipient))
            bk = cipher.encrypt(msg_key) #+h.digest())
            bridge_keys.append(bk)
        
        return (bloom_header.tobytes() +
                bytes([len(recipients)]) + b''.join(bridge_keys) +
                ciphertext)

    def decrypt(self, blob, peer):
        # To decrypt, first slice off the bloom_header, and check if there's a chance
        # one of the bridge_keys can be decrypted.
        num_bridge_keys, offset = self._num_keys_to_check(blob, peer)
        if num_bridge_keys == 0:
            return None

        log.debug("We have %d bridge keys to check", num_bridge_keys)

        current_index = offset
        msg_offset = offset + (128 * num_bridge_keys)

        msg = None
        bridge_key = None
        for i in range(0, num_bridge_keys):
            ciphertext = blob[current_index:current_index+128]
            current_index += 128
            try:
                bridge_key = self._decrypt_bridge_key(ciphertext, peer.key)
            except ValueError:
                continue

            try:
                if bridge_key:
                    log.debug("trying to decrypt with bridge_key %s", codecs.encode(bridge_key, 'hex'))
                    msg = self._decrypt_message_object(blob[msg_offset:], bridge_key)
                    break
            except UnicodeDecodeError:
                pass

        if not bridge_key:
            return None

        return msg

    def _num_keys_to_check(self, blob, peer):
        filter_size = settings.MSG_BLOOM_FILTER_SIZE 

        bloom_end_index = int(filter_size/8)
        bloom_header = bitarray()
        bloom_header.frombytes(blob[:bloom_end_index])

        if (bloom_hash(peer.address, filter_size) | bloom_header) != bloom_header:
            return (0, None)

        num_bridge_keys = int.from_bytes(blob[bloom_end_index:bloom_end_index+1],
                byteorder='little')
        return (num_bridge_keys, bloom_end_index+1)

    def _decrypt_bridge_key(self, ciphertext, priv_key):
        #dsize = SHA.digest_size
        #sentinel = Random.new().read(15+dsize) # Let's assume that average data length is 15

        cipher = PKCS1_OAEP.new(priv_key)
        bridge_key = cipher.decrypt(ciphertext)# sentinel)
        bk = bridge_key #[:-dsize]
        return bk

    def _decrypt_message_object(self, ciphertext, key):
        iv = ciphertext[0:Blowfish.block_size]
        cipher = Blowfish.new(key, Blowfish.MODE_CBC, iv)
        #log.debug("length of blowfish ciphertext ", len(blob[current_index+Blowfish.block_size:]))
        message_text = cipher.decrypt(ciphertext[Blowfish.block_size:])
        num_padding = int.from_bytes([message_text[-1]], byteorder='little')
        message_text = message_text[:-1 * num_padding]
        return Message.from_bytes(message_text)

class MessageRouter(object):
    """
    Given a recipient and a message, and the fact that the recipient might
    not currently be online, use the overlap in social contacts and their
    reliability to work out the best destination to be sent the message on their
    behalf.
    """

    def connection_ranking(self, recipients, online_peers):
        return set(online_peers) - set(recipients)

    def route(self, recipients, online_peers, message):
        """
        Iterator will keep returning potential destinations to
        receive the message. Might not be the actual recipient if they are
        offline, but it's a guess at who might be valid path to get the message
        to them.
        """

        # First, try to send to all recipients we think are online
        for r in recipients:
            if r in online_peers:
                yield r

        for o in self.connection_ranking(recipients, online_peers):
            yield o


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

class PeerOffline(Exception): pass


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

        if p.connected:
            p.handle_message(message)
        else:
            raise PeerOffline
        return True

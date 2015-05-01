import hashlib
import logging

from Crypto.PublicKey import RSA

from tenet.message import (
        Message,
        DictTransport, MessageRouter, MessageSerializer, MessageTypes
        )

log = logging.getLogger(__name__)


class Peer(object):

    def __init__(self, address):
        self.address = address
        self.friends = []

        self.key = RSA.generate(1024)

        self.my_messages = []

        self.post_office = {}

        # Store blobs by their content hash, to
        # allow deduplication
        self.blobs_by_content_hash = {}
        # Store blobs in order to allow requests for updates since T=t
        self.ordered_blobs = []
        # Store blobs by local id/counter, probably not needed if we have
        # ordered blobs
        self.blobs_by_local_id = {}

        # The local id of the last message we recieved from the network
        self.id_counter = 0
        log.debug("Created peer {}".format(self.address))

    def handle_message(self, blob):
        serializer = MessageSerializer()
        msg = serializer.decrypt(blob, self)

        # Should store the blob, not the decrypted content
        if self.store_message(blob):
            log.debug("{} recieved a duplicate message from {}, it said '{}'".format(self, msg.author, msg.data.get('text')))
        else:
            log.debug("{} received a message from {}, it said '{}'".format(self, msg.author, msg.data.get('text')))

    def store_message(self, blob):
        """ Returns true is this message already exists """
        self.id_counter += 1
        self.ordered_blobs.append((self.id_counter, blob))
        self.blobs_by_local_id[self.id_counter] = blob

        md5sum = hashlib.md5()
        md5sum.update(blob)
        digest = md5sum.digest()
        if digest in self.blobs_by_content_hash:
            return True
        self.blobs_by_content_hash[digest] = blob
        return False

    def send(self, msg, transport, router):
        serializer = MessageSerializer()
        message_blobs = serializer.encrypt(msg)
        for recipients, blob in message_blobs:
            for r in recipients:
                # Allow router to inspect message
                dest = router.route(r, msg)
                # Transport only sees encrypted blob
                transport.send_to(dest, blob)

    def check_pending_messages(self, peer_address):
        """ Do I have any messages for peer_address? """
        # Need a way of knowing which messages have been sent to peers already
        # - Peers could keep track of which messages have already been sent to each other peer?
        # - Requesting peer could also say when they last asked.
        # - Needs to be robust against peers being silly.
        if peer_address not in self.post_office:
            return None

    def __str__(self):
        return "Peer %s" % self.address

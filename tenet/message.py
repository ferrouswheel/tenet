
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

    # Let a peer know another peer is behaving badly
    GOSSIP_GOOD = msg_count()
    # Let a peer know another peer is reputable
    GOSSIP_BAD = msg_count()


class Message(object):
    """ The abstract message class for sending updates to peers.

    """

    def __init__(self, author, recipients, msg_type, **data):
        self.author = author
        self.recipients = recipients
        self.message_type = msg_type
        self.data = data


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
        return [(msg.recipients, msg)]


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

    def send_to(self, dest_address, message):
        p = self.peers.get(dest_address)
        if p is None:
            raise InvalidAddress(dest_address)
        p.handle_message(message)
        return True

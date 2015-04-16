import random

from tenet.message import (
        Message,
        DictTransport, MessageRouter, MessageSerializer, MessageTypes
        )

names = ['Ariel', 'Boris', 'Carrie', 'Daniel', 'Ezekiel', 'Fiona', 'Harold', 'Indiana']
hosts = ['example.com', 'gmail.com', 'robot.com', 'zombo.com', 'yahoo.com', 'geocities.com']

class Peer(object):

    def __init__(self, address):
        self.address = address
        self.friends = []

        self.my_messages = []

        self.post_office = {}

    def handle_message(self, msg):
        self.my_messages.append(msg)
        print("{} recieved a message from {}, it said {}!".format(self, msg.author, msg.data.get('text')))

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

def generate_random_peers(number=100):
    for i in range(0, number):
        address = random.choice(names) + '_' + str(i) + '@' + random.choice(hosts)
        yield Peer(address)

def random_friendships(peers, density=0.1):
    x = len(peers)
    links = int(x*x*density)
    for i in range(0, links):
        p1 = random.choice(peers)
        p2 = None
        while p2 is None or p1 == p2:
            p2 = random.choice(peers)

        # TODO exchange keys too
        p1.friends.append(p2)
        p2.friends.append(p1)

        #print('{} and {} are now friends'.format(p1, p2))


def random_message(transport, router, peers):
    sender = random.choice(peers)
    recipient = None
    while recipient is None or recipient == sender:
        recipient = random.choice(peers)

    msg = Message(sender, [recipient.address], MessageTypes.MESSAGE, text="Hello {}!".format(recipient))

    serializer = MessageSerializer()
    messages = serializer.encrypt(msg)
    for recipients, message in messages:
        for r in recipients:
            dest = router.route(r, msg)
            transport.send_to(dest, msg)


def random_messaging(transport, router, peers, num_messages=1000):
    for i in range(0, num_messages):
        random_message(transport, router, peers)

        
if __name__ == '__main__':
    peers = [x for x in generate_random_peers()]
    [print(x) for x in peers]
    random_friendships(peers)

    transport = DictTransport()
    for p in peers:
        transport.peers[p.address] = p

    router = MessageRouter()

    random_messaging(transport, router, peers)

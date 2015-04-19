import random
import hashlib
import simpy
import networkx as nx

from Crypto.PublicKey import RSA

from tenet.message import (
        Message,
        DictTransport, MessageRouter, MessageSerializer, MessageTypes
        )


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
        print("Created peer {}".format(self.address))

    def handle_message(self, blob):
        serializer = MessageSerializer()
        msg = serializer.decrypt(blob, self)

        # Should store the blob, not the decrypted content
        if self.store_message(blob):
            print("{} recieved a duplicate message from {}, it said '{}'".format(self, msg.author, msg.data.get('text')))
        else:
            print("{} received a message from {}, it said '{}'".format(self, msg.author, msg.data.get('text')))

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

    def simulate(self, router, transport, env):
        while True:
            wait_duration = random.randint(0,90)
            yield env.timeout(wait_duration)
            random_message(transport, router, self)

    def __str__(self):
        return "Peer %s" % self.address


def random_address(i):
    names = ['Ariel', 'Boris', 'Carrie', 'Daniel', 'Ezekiel', 'Fiona', 'Harold', 'Indiana']
    hosts = ['example.com', 'gmail.com', 'robot.com', 'zombo.com', 'yahoo.com', 'geocities.com']

    return random.choice(names) + '_' + str(i) + '@' + random.choice(hosts)


def generate_random_peers(number=100):
    for i in range(0, number):
        yield Peer(random_address(i))


def random_friendships(peers, G=None, density=0.1):
    x = len(peers)
    links = int(x*x*density)
    for i in range(0, links):
        p1 = random.choice(peers)
        p2 = None
        while p2 is None or p1 == p2:
            p2 = random.choice(peers)

        G.add_edge(p1.address, p2.address)
        # TODO exchange keys too
        p1.friends.append(p2)
        p2.friends.append(p1)

        #print('{} and {} are now friends'.format(p1, p2))


def random_message(transport, router, sender):
    recipient = None
    if not sender.friends:
        print("{} has no friends :-(".format(sender))
        return
    while recipient is None or recipient == sender:
        recipient = random.choice(sender.friends)

    msg = Message(sender, [recipient], MessageTypes.MESSAGE, text="Hello {}!".format(recipient))

    sender.send(msg, transport, router)


def random_messaging(transport, router, peers, num_messages=1000):
    for i in range(0, num_messages):
        sender = random.choice(peers)
        random_message(transport, router, sender)

def gen_social_graph_1(num_people=10):
    G=nx.Graph()

    peers = [x for x in generate_random_peers(num_people)]
    [print(x) for x in peers]

    for p in peers:
        G.add_node(p.address)
    random_friendships(peers, G)

    return (peers, G)


def gen_social_graph_2(num_people=10):
    G=nx.random_geometric_graph(num_people,0.125)

    peer_by_id = {}
    for n in G.nodes():
        peer_by_id[n] = Peer(random_address(n))

    for e in G.edges():
        f1 = peer_by_id[e[0]]
        f2 = peer_by_id[e[1]]
        f1.friends.append(f2)
        f2.friends.append(f1)

    return peer_by_id.values(), G


def draw_graph(G):
    try:
        from networkx import graphviz_layout
    except ImportError:
        raise ImportError("This example needs Graphviz and either PyGraphviz or Pydot")

    import matplotlib.pyplot as plt
    plt.figure(1, figsize=(8,8))
    # layout graphs with positions using graphviz neato
    #pos=nx.graphviz_layout(G, prog="neato")
    pos=nx.get_node_attributes(G,'pos')
    nx.draw_networkx_edges(G,pos,alpha=0.4)
    nx.draw_networkx_nodes(G,pos,
                           node_size=80,
                           cmap=plt.cm.Reds_r)
    #nx.draw(G,
         #pos,
         #node_size=40,
         ##node_color=c,
         #vmin=0.0,
         #vmax=1.0,
         #with_labels=False
         #)
    plt.savefig("tenet.png",dpi=75)

        
if __name__ == '__main__':
    #(peers, G) = gen_social_graph_1(10)
    (peers_iter, G) = gen_social_graph_2(20)
    peers = list(peers_iter)

    print("graph has %d nodes with %d edges"\
          %(nx.number_of_nodes(G),nx.number_of_edges(G)))
    print(nx.number_connected_components(G),"connected components")

    transport = DictTransport()
    for p in peers:
        transport.peers[p.address] = p

    router = MessageRouter()

    env = simpy.Environment()
    for p in peers:
        env.process(p.simulate(router, transport, env))

    env.run(until=1000)

    #random_messaging(transport, router, peers)

    draw_graph(G)

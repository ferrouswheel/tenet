import random
import networkx as nx

from tenet.message import (
        Message,
        DictTransport, MessageRouter, MessageSerializer, MessageTypes
        )


class Peer(object):

    def __init__(self, address):
        self.address = address
        self.friends = []

        self.my_messages = []

        self.post_office = {}

    def handle_message(self, msg):
        self.my_messages.append(msg)
        print("{} recieved a message from {}, it said '{}'".format(self, msg.author, msg.data.get('text')))

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

def gen_social_graph_1():
    G=nx.Graph()

    peers = [x for x in generate_random_peers()]
    [print(x) for x in peers]

    for p in peers:
        G.add_node(p.address)
    random_friendships(peers, G)

    return (peers, G)


def gen_social_graph_2():
    G=nx.random_geometric_graph(200,0.125)

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
    #(peers, G) = gen_social_graph_1()
    (peers_iter, G) = gen_social_graph_2()
    peers = list(peers_iter)

    print("graph has %d nodes with %d edges"\
          %(nx.number_of_nodes(G),nx.number_of_edges(G)))
    print(nx.number_connected_components(G),"connected components")

    transport = DictTransport()
    for p in peers:
        transport.peers[p.address] = p

    router = MessageRouter()

    random_messaging(transport, router, peers)

    draw_graph(G)

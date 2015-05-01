import random
import logging
import networkx as nx

from tenet.message import (
        Message,
        DictTransport, MessageRouter, MessageSerializer, MessageTypes
        )
from tenet.peer import Peer


log = logging.getLogger(__name__)


class SimulatedPeer(object):

    def __init__(self, peer):
        self.peer = peer

    def simulate(self, router, transport, env):
        while True:
            wait_duration = random.randint(0,90)
            yield env.timeout(wait_duration)
            self.random_message(transport, router)

    def random_message(self, transport, router):
        sender = self.peer
        recipient = None
        if not sender.friends:
            log.debug("{} has no friends :-(".format(sender))
            return
        while recipient is None or recipient == sender:
            recipient = random.choice(sender.friends)

        msg = Message(sender, [recipient], MessageTypes.MESSAGE, text="Hello {}!".format(recipient))

        sender.send(msg, transport, router)


def random_address(i):
    names = ['Ariel', 'Boris', 'Carrie', 'Daniel', 'Ezekiel', 'Fiona', 'Harold', 'Indiana']
    hosts = ['example.com', 'gmail.com', 'robot.com', 'zombo.com', 'yahoo.com', 'geocities.com']

    return random.choice(names) + '_' + str(i) + '@' + random.choice(hosts)


def generate_random_peers(number=100):
    for i in range(0, number):
        p = Peer(random_address(i))
        yield SimulatedPeer(p)


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

        #log.debug('{} and {} are now friends'.format(p1, p2))


def gen_social_graph_1(num_people=10):
    G=nx.Graph()

    peers = [x for x in generate_random_peers(num_people)]
    [log.debug(x) for x in peers]

    for p in peers:
        G.add_node(p.address)
    random_friendships([p.peer for p in peers], G)

    return (peers, G)


def gen_social_graph_2(num_people=10):
    G=nx.random_geometric_graph(num_people,0.125)

    peer_by_id = {}
    for n in G.nodes():
        peer_by_id[n] = SimulatedPeer(Peer(random_address(n)))

    for e in G.edges():
        f1 = peer_by_id[e[0]]
        f2 = peer_by_id[e[1]]
        f1.peer.friends.append(f2.peer)
        f2.peer.friends.append(f1.peer)

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

        

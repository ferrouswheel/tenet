import random
import logging
import networkx as nx

from tenet.message import (
        Message,
        DictTransport, MessageSerializer, MessageTypes
        )
from tenet.peer import Peer, Friend
from tenet.utils import weighted_choice


log = logging.getLogger(__name__)


class SimulatedPeer(object):

    def __init__(self, peer):
        self.peer = peer
        self.connected = True

    def simulate(self, transport, env):
        actions = [('friend_post', 4), ('send_msg', 4)]
        while True:
            available_actions = list(actions)
            if self.peer.connected:
                available_actions.append(('disconnect', 2))
            else:
                # NOTE: maybe simulate offline posts
                # so that connection behaviour and a sudden egress of messages
                # doesn't mess things up
                available_actions = [('connect', 1), ('none', 3)]

            a = weighted_choice(available_actions)

            if a == 'send_msg':
                log.debug("{} will send a message.".format(self.peer))
                self.random_message(transport)
            elif a == 'friend_post':
                log.debug("{} will make a post.".format(self.peer))
                self.random_post(transport)
            elif a == 'disconnect':
                log.info("{} disconnecting".format(self.peer))
                self.peer.connected = False
            elif a == 'connect':
                log.info("{} reconnecting".format(self.peer))
                self.peer.connected = True
                self.peer.on_connect(transport)

            wait_duration = random.randint(1,4)
            yield env.timeout(wait_duration)


    def random_post(self, transport):
        sender = self.peer
        recipients = set()
        if not sender.friends:
            log.debug("{} has no friends :-(".format(sender))
            return

        num_recipients = random.randint(1, len(list(sender.friends.values())))
        while len(recipients) < num_recipients:
            r = random.choice(list(sender.friends.values()))
            recipients.add(r)

        msg = Message(sender.address, [r.address for r in recipients], MessageTypes.SHARE, text="This is a general post to mah friends!")

        sender.send(msg, transport)

    def random_message(self, transport):
        sender = self.peer
        recipient = None
        if not sender.friends:
            log.debug("{} has no friends :-(".format(sender))
            return
        while recipient is None or recipient == sender:
            recipient = random.choice(list(sender.friends.values()))

        msg = Message(sender.address, [recipient.address], MessageTypes.MESSAGE, text="Hello {}!".format(recipient))

        sender.send(msg, transport)


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
        p1.friends[p2.address] = Friend(p2.address, p2.key)
        p2.friends[p1.address] = Friend(p1.address, p1.key)

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
    G=nx.random_geometric_graph(num_people,0.325)

    peer_by_id = {}
    for n in G.nodes():
        peer_by_id[n] = SimulatedPeer(Peer(random_address(n)))

    for e in G.edges():
        p1 = peer_by_id[e[0]]
        p2 = peer_by_id[e[1]]
        p1.peer.friends[p2.peer.address] = Friend(p2.peer.address, p2.peer.key)
        p2.peer.friends[p1.peer.address] = Friend(p1.peer.address, p1.peer.key)

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

        

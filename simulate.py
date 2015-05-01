#!/usr/bin/env python3

import simpy
import networkx as nx

from tenet.simulation import gen_social_graph_2, draw_graph

from tenet.message import (
        DictTransport, MessageRouter
        )

if __name__ == '__main__':
    #(peers, G) = gen_social_graph_1(10)
    (peers_iter, G) = gen_social_graph_2(20)
    simulated_peers = list(peers_iter)

    print("graph has %d nodes with %d edges"\
          %(nx.number_of_nodes(G),nx.number_of_edges(G)))
    print(nx.number_connected_components(G),"connected components")

    transport = DictTransport()
    for sp in simulated_peers:
        transport.peers[sp.peer.address] = sp.peer

    router = MessageRouter()

    env = simpy.Environment()
    for sp in simulated_peers:
        env.process(sp.simulate(router, transport, env))

    env.run(until=1000)

    #random_messaging(transport, router, peers)

    draw_graph(G)

#!/usr/bin/env python3
import logging
from logging.config import dictConfig
import simpy
import networkx as nx

from tenet.simulation import gen_social_graph_2, draw_graph
from tenet.utils import ColorFormatter

from tenet.message import (
        DictTransport, MessageRouter
        )


logging_config = dict(
    version = 1,
    disable_existing_loggers = False,
    formatters = {
        'f': {
            '()': ColorFormatter,
            'format':
              #'%(asctime)s %(name)-16s %(levelname)-6s %(message)s',
              '$COLOR%(levelname)-8s$RESET %(asctime)s $BOLD$GREEN%(name)-16s$RESET %(message)s'
              }
        },
    handlers = {
        'h': {'class': 'logging.StreamHandler',
              'formatter': 'f',
              'level': logging.DEBUG}
        },
    #loggers = {
        #'root': {'handlers': ['h'],
                 #'level': logging.DEBUG}
        #},
    root = {
      "level": logging.DEBUG,
      "handlers": ['h']
    }
)

dictConfig(logging_config)

log = logging.getLogger(__name__)


if __name__ == '__main__':
    #(peers, G) = gen_social_graph_1(10)
    (peers_iter, G) = gen_social_graph_2(20)
    simulated_peers = list(peers_iter)

    log.info("Generated social graph has %d nodes with %d edges"\
          %(nx.number_of_nodes(G),nx.number_of_edges(G)))
    log.info(str(nx.number_connected_components(G)) + " connected components")

    # Initialise the dummy transport for simulation
    transport = DictTransport()
    for sp in simulated_peers:
        transport.peers[sp.peer.address] = sp.peer

    env = simpy.Environment()
    for sp in simulated_peers:
        env.process(sp.simulate(transport, env))

    env.run(until=1000)

    for sp in simulated_peers:
        log.info(str(sp.peer) + " metrics: " + repr(sp.peer.metrics()))

    draw_graph(G)

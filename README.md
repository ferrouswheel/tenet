tenet
=====

A protocol for a p2p social network with reference implementation.

tenet makes the following assumptions, it's long term viability in large part
will depend on these:

* People will have personal "smart" devices connected to the internet for
  a large proportion of the time.
* Centralisation of societies social connections, meta data, and our own
  personal interactions with friends will become more politicised. We will want
  to assert some control over just who has access to our relationship data.

It would be nice if this became true:

* Devices will get their own ipv6 address which is directly accessible to other
  devices.
  
However, supporting non-direct transports will mean this isn't strictly
necessary. A third party server could act as a dumb router, or indeed PGP
envrypted blobs could be sent via email.

## A distributed social fabric

The basic idea is that every node/user in the social network will store and
download not only the updates that are addressed to them, but also the updates
addressed to their friends, or friends of friends. The idea being partly that
your social network should be more trustworthy than a centralised server that
gets to decide who was root access to the world's online social interactions.
It also distributes the risk, because even if some disconnected part of the p2p
network is compromised in some way (social or techical) it is only a `risk` if
they are part of your social group.

To summarise: we require that each user be a node participating in distributing
content, including the content they can’t read.

How do we determine which content can be read? All messages between peers are
encrypted using PGP. Each message has a header and key preamble that allows
a message to be addressed to multiple recipients with different PGP key pairs
(this is an assumption I've made that appears to be "safe", but I'm keen to
here from cryptographers as to whether it is)

## Alternatives

[Diaspora]() is probably the most well known social network making an attempt to
replace centralised services. However, Diaspora is still fundamentally about
servers in control of your information. Diaspora could be consider more
a "federated" solution to social networking, where tenet is meant to be "peer
to peer" in the same way that we think of bittorrent and other file sharing
applications to be.

(Could a bridge be possible between diaspora and tenet? It'd be better to
allow connectivity than exclude protocols unnnecessarily.)

[Camlistore]() is promising appraoch for allowing people to sharing a blob
filestystem, which may be useful for guiding tenet.

A number of p2p social networks have also come and gone, but have been based on
the idea of people running things on their local computer. The reality however
is that many of us have our phones on more than our computers, and as
technology progresses, phones will only become cheaper, more efficient,
connected, and ubiquitous.

I also believe that usability is fundamental. It doesn't matter how cool the
p2p technology, encryption, or geekiness of tenet is, if non-techs can't
easily use the platform... they won't. Being in control of one's social data
should only by one small part of the offering.

## Technical Assumption

* [Transience]() - Each node will only store a recent history of shared memory/content. It’s up
  to users to connect frequently enough to get all updates. This should be
  fine, since very rarely do we have to look back over other people's posts
  unless it's something we remember seeing. This has the side effect that
  "forgetting" is intrinsic to the social network. If you were not around at
  the time when stuff was shared, then you can't go back and try and dig up
  dirt on people after the fact.
* [Transports]() - Most people’s phones won’t be able to run internet
  accessible servers, so we'll need a router. Initially we could use dumb
  websockets which just allow the user to connect to others. This transport layer
  should be easily replaceable so long as it provides the hooks expected by
  tenet.
* [Multi-party decryption]() We can encrypt blobs that are addressed to
  multiple recipients, and do this without duplicating and encrypting the
  entire message for each recipient.



### Multi-party Decryption

What follows is a message format that allows for decryption by multiple parties
using different keys.

When friend requests are accepted, each user exchange their public keys. If
Alice wishes to send a message to Bob, and to Carol, then Alice sends a message
of the following format.

If for some reason this design can't work, or we can’t have shared decryption in general,
then we’ll need to store N encrypted blobs for N users.  This is much less efficient,
and may make the exchange of updates much less scalable for real-world social
networks. Although bandwidth is alwasy getting cheaper.

```
[ bloom header ][ C_key1 ][ C_key2 ]…[ C_keyN ][ shared encrypted blob]
```

NOTE: I’m not sure if multiple encryptions of the same/similar data would make the
key blocks leak information making it easier to bruteforce the shared blob.
I can’t determine how this could happen, but maybe it would.

**Bloom header**

This is the a bloom filter using the hashes of the public keys used to encrypt
`C_key1`..`C_keyN`.

**`C_key1 C_keyN`**

These each contain the same encrypted data with the private key for the "shared
encrypted blob".

- the id of sender
- the message counter for the sender (or a GUID? no. have GUID for update encrypted, update message counter)

things to keep in mind:
- lots of encryption keys to check could make things slow, especially in the case of a big friend list and someone not actually having access (which means they’d have to check all potential keys). Maybe use bloom filters to work out if a given key probably has access ahead of time? (to create bloom filter, just add all keys that are allowed, if receiver sees their key not there, then skip decode).
- also, the encrypted keys to allow further decryption of the blob shouldn’t be huge certificates, would be better if they were smaller keys like aes: http://crypto.stackexchange.com/questions/9723/vulnerabilities-if-encrypting-the-same-data-with-2-different-keys
- if there are too many recipients, then the update can be duplicated into two separate messages.
- we don’t want to leak people’s friend list if at all possible.

### Account Recovery

account recovery - because you rely on your key, then if it’s missing or stolen you’ll lose everything. fix this by having trusted friends with a revocation ability or with a backup of your social network? may need in person confirmation process.

## Peer Actions

use a basic format like json for data before encryption.

update types:

- new post (optional with attachment) - largest size possible? maybe use a linked list of data blobs if needed
- new comment (optional with attachment)
- friend request
- friend accept
- friend delete (or silently stop providing updates)

### how to post an update:

encrypt update body using header/keys/blob format above.

### how to retrieve updates:

- query bridge/router to see who is online. (or in future with IPv6 and phones that can run servers and accept connections, just contact your friend IPs directly? or ask friends if they know the IP for other friends)
- for each successful ping, based on order of reply:
  - for each friend, create a fingerprint for month history (number of updates per day, in UTC including a md5sum for each day’s update ids), and send to friend
  - ask for any posts addressed to me

### friend request/accept/delete requires direct acknowledgement from receiver. other updates can be shared.

shared c code http://stackoverflow.com/questions/18334547/how-to-use-the-same-c-code-for-android-and-ios

transport layers
While bittorrent uses direct TCP/UDP, our aim is to able to practically run
clients on user's phones. Most cell phones however cannot have direct externally
initiated connections.

there could be a number of swappable transport layers:
- one is direct connection to other hosts, based on a static IP address (or with ipv6 the devices unique address?). discovery of IPs could by via asking friends, and last host ID -> IP would be cached. each host would also send an announcement updateIP message to hosts it knew about. if on a local wifi network, can we implement upnp to open connections the same way bittorrent does?
- use a routing server that does nothing more than route messages between hosts (it doesn’t store them!). this is more feasible for mobile connections that probably don’t have a fixed IP peers can connect to. and that mixes communications between hosts so the network can’t be inferred by who is sending packets to who (well, they could track the traffic of the connections and correllate messages maybe? use TLS?)
- email (formatted pgp messages sent to user’s email, wouldn’t need other hosts to buffer messages for them since inbox would fulfill that role)
- bitcoin block chain?

all hosts would have to know how to communicate with hosts on any transport layer.

web server
- use bonjour/avahi/mdns to announce a a service joel-tenet.local or something similar. show a login key on cell phone, and pair.
- one problem is we can’t use https without having a self-signed certificate.

account creation
- to create an account:
generate user id
set message count to 0
generate pgp key pair with password
fill in profile information
no messages need to be sent

initiate friend request
get the id of user via qr code, url, or manually entering it
create message with:
{ sender: userid,
  pubkey: “public keyaasdadasda”,
  

receive friend request

For cell users:
http://pagekite.net/wiki/Floss/FreedomAndPrivacy/
(or custom server for routing messages, using websockets or something else that
 works over cell-acceptable protocols)

For people behind home routers:
https://github.com/miniupnp/miniupnp/blob/master/miniupnpc/testupnpigd.py

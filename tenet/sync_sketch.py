
import uuid
import hashlib

class Friend(object):

    def __init__(self, msgs):
        self.tenetid = uuid.uuid1()
        self.msgs = msgs

    def get_pending_messages(self, finger_print):
        my_fp = finger_history(self.msgs)
        for my_f, f in zip(my_fp, finger_print):
            if my_f[1] != 0 and my_f != f:
                print my_f, f


msgs = [ ('bob', 123), ('bob', 200), ('bob', 300), ('bob', 400) ]

friends = [
 Friend([ msgs[0], msgs[3] ]),
 Friend([ msgs[0], msgs[2] ]),
 Friend([ msgs[0], msgs[1] ]),
]

existing_messages = [ ('bob', 10), ('bob', 170), ('bob', 200) ]

class ObjectStore(object):

    def __init__(self):
        self.objects = []

    def add(self, blobs):
        self.objects.extend(blobs)

    def finger_history(self):
        current_time = 401
        msg_pos = 0
        updates_fingerprint = []
        for t in range(40, 0, -1):
            start_t = current_time - (t*10)
            end_t = current_time - ((t-1)*10)
            
            count = 0
            m = hashlib.md5()

            while ( msg_pos < len(self.existing_messages)
                    and self.existing_messages[msg_pos][1] < end_t
                    and self.existing_messages[msg_pos][1] >= start_t):
                m.update(str(self.existing_messages[msg_pos]))
                count += 1
                msg_pos += 1

            updates_fingerprint.append((start_t, count, m.hexdigest() if count else None))
        return updates_fingerprint

all_msgs = []

obj_store = ObjectStore()
obj_store.add(existing_messages)

for f in friends:
    print "friend"
    print f.get_pending_messages(obj_store.finger_history())


import socket, json, time

def send(req):
    s = socket.create_connection(('127.0.0.1', 53421), timeout=2)
    s.sendall((json.dumps(req) + '\n').encode('utf-8'))
    resp = s.recv(8192).decode('utf-8')
    s.close()
    print(resp)
    return resp

# list devices (copy ids)
send({"cmd":"list_devices"})

# open both input & output so loopback works (use your device ids)
send({"cmd":"open","input":6,"output":1,"sr":48000})

# send an MFSK frame
send({
 "cmd":"send_frame",
 "mode":"mfsk",
 "mfsk_m":4,
 "mfsk_center":1500.0,
 "mfsk_spacing":200.0,
 "symbol_len_ms":80,
 "preamble_repeats":3,
 "payload_bits":"1011001110001010"
})


# Optionally start tone fallback
send({"cmd":"start_tx","tone_freq":700})
time.sleep(1)
send({"cmd":"stop_tx"})
send({"cmd":"close"})

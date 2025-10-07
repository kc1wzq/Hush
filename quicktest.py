# quick_test.py
import socket, json, time

def send(req):
    s = socket.create_connection(('127.0.0.1', 53421), timeout=2)
    s.sendall((json.dumps(req) + '\n').encode('utf-8'))
    data = b''
    while True:
        chunk = s.recv(8192)
        if not chunk: break
        data += chunk
        if b'\n' in chunk: break
    s.close()
    print("<- ", data.decode().strip())

# device IDs from your run: input=1, output=6
send({"cmd":"open","input":6,"output":1,"sr":48000})
time.sleep(0.05)

send({
 "cmd":"send_frame",
 "mode":"mfsk",
 "mfsk_m":4,
 "mfsk_center":1500.0,
 "mfsk_spacing":200.0,
 "symbol_len_ms":100,
 "preamble_repeats":6,
 "payload_bits":"1011001110001010"
})
time.sleep(0.5)
send({"cmd":"status"})
send({"cmd":"close"})

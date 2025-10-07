import socket, json, time

HOST='127.0.0.1'
PORT=53421

def send(req, timeout=2.0):
    s = socket.create_connection((HOST, PORT), timeout=timeout)
    payload = json.dumps(req) + '\n'
    s.sendall(payload.encode('utf-8'))
    # read until newline
    data = b''
    while True:
        chunk = s.recv(4096)
        if not chunk:
            break
        data += chunk
        if b'\n' in chunk:
            break
    s.close()
    try:
        text = data.decode('utf-8').strip()
    except:
        text = repr(data)
    print("->", req)
    print("<-", text)
    return text

# quick test sequence:
send({"cmd":"list_devices"})
# FOR DEV: radio input (7,4) loopback (6,1)
INPUT_ID = 6
OUTPUT_ID = 1

send({"cmd":"open", "input": INPUT_ID, "output": OUTPUT_ID, "sr": 48000})
time.sleep(0.1)


# send an MFSK frame
send({
 "cmd":"send_frame",
 "mode":"mfsk",
 "mfsk_m":64,
 "mfsk_center":1500.0,
 "mfsk_spacing":15.0,
 "symbol_len_ms":400,
 "preamble_repeats":4,
 "payload_bits":"010101000100010101010011010101000100100101001110010001110010000001010100010001010101001101010100010010010100111001000111001000000100100001010101010100110100100000100000010100000101001001001111010101000100111101000011010011110100110000100000010010110100001100110001010101110101101001010001010101000100010101010011010101000100100101001110010001110010000001010100010001010101001101010100010010010100111001000111001000000100100001010101010100110100100000100000010100000101001001001111010101000100111101000011010011110100110000100000010010110100001100110001010101110101101001010001"
})
# wait a little so worker can process
time.sleep(0.5)
send({"cmd":"status"})
send({"cmd":"close"})

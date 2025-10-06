# control_client.py
import socket, json, time, sys

HOST = '127.0.0.1'
PORT = 53421

def send(req):
    s = socket.create_connection((HOST, PORT), timeout=2)
    s.sendall((json.dumps(req) + '\n').encode('utf-8'))
    resp = s.recv(8192).decode('utf-8')
    s.close()
    return resp

if __name__ == '__main__':
    print("Listing devices...")
    print(send({"cmd":"list_devices"}))

    # >>> pick ids from the list printed by your hush core above.
    # Example: use device 1 as input and 2 as output — CHANGE these to match your system.
    input_id = 2
    output_id = 5

    # Open streams
    print("Opening streams (change input_id/output_id as needed)...")
    print(send({"cmd":"open","input":input_id,"output":output_id,"sr":48000}))

    # Start tone
    print("Starting TX tone at 700 Hz for 4s...")
    print(send({"cmd":"start_tx","tone_freq":700}))
    time.sleep(4)

    # Stop tone
    print("Stopping TX...")
    print(send({"cmd":"stop_tx"}))

    # Query status
    print("Status:")
    print(send({"cmd":"status"}))

    # Close streams
    print("Closing streams...")
    print(send({"cmd":"close"}))
    print("Done.")

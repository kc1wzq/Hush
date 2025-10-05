// src/main.cpp
#include <iostream>
#include <thread>
#include <atomic>
#include <vector>
#include <string>
#include <sstream>
#include <chrono>
#include <cstring>
#include <cmath>
#include <mutex>

#ifdef _WIN32
  #define WIN32_LEAN_AND_MEAN
  #include <winsock2.h>
  #include <ws2tcpip.h>
  #pragma comment(lib, "ws2_32.lib")
  typedef SOCKET socket_t;
  #define CLOSE_SOCKET(s) closesocket(s)
  #define ssize_t int
#else
  #include <netinet/in.h>
  #include <unistd.h>
  #include <arpa/inet.h>
  typedef int socket_t;
  #define CLOSE_SOCKET(s) close(s)
#endif

#include "portaudio.h"
#include "ringbuffer.hpp"
#include <nlohmann/json.hpp>
using json = nlohmann::json;

constexpr int CONTROL_PORT = 53421;
constexpr int DEFAULT_SR = 48000;
constexpr int CHANNELS = 1; // mono audio for Hush prototypes
constexpr size_t RINGBUF_CAP = 1<<17; // ~131k floats (~0.5s at 48k mono)

struct CoreState {
    PaStream* stream = nullptr;
    int input_device = -1;
    int output_device = -1;
    int sample_rate = DEFAULT_SR;
    std::atomic<bool> running{false};
    std::atomic<bool> tx_enabled{false};
    std::atomic<float> tx_tone_freq{600.0f};
    FloatRingBuffer ring;
    std::mutex state_mtx;
    float last_rx_rms = 0.0f;
    CoreState(): ring(RINGBUF_CAP) {}
} core;

static int audioCallback(const void* inputBuffer, void* outputBuffer,
                 unsigned long framesPerBuffer,
                 const PaStreamCallbackTimeInfo* timeInfo,
                 PaStreamCallbackFlags statusFlags,
                 void* userData)
{
    CoreState* s = (CoreState*)userData;
    const float* in = (const float*)inputBuffer;
    float* out = (float*)outputBuffer;
    if (inputBuffer == nullptr) {
        static std::vector<float> zeros;
        zeros.assign(framesPerBuffer, 0.0f);
        s->ring.push(zeros.data(), zeros.size());
    } else {
        s->ring.push(in, framesPerBuffer * CHANNELS);
    }

    if (s->tx_enabled.load()) {
        static double phase = 0.0;
        double freq = s->tx_tone_freq.load();
        double sr = s->sample_rate;
        for (unsigned long i=0;i<framesPerBuffer;i++) {
            float v = (float)sin(phase);
            out[i] = v * 0.3f;
            phase += 2.0*M_PI*freq/sr;
            if (phase > 2.0*M_PI) phase -= 2.0*M_PI;
        }
    } else {
        memset(out, 0, sizeof(float)*framesPerBuffer*CHANNELS);
    }

    return paContinue;
}

void list_devices(json& resp) {
    int num = Pa_GetDeviceCount();
    json arr = json::array();
    for (int i=0;i<num;i++) {
        const PaDeviceInfo* info = Pa_GetDeviceInfo(i);
        const PaHostApiInfo* host = Pa_GetHostApiInfo(info->hostApi);
        json d;
        d["id"] = i;
        d["name"] = info->name;
        d["maxInputChannels"] = info->maxInputChannels;
        d["maxOutputChannels"] = info->maxOutputChannels;
        d["defaultSampleRate"] = info->defaultSampleRate;
        d["hostApi"] = host->name;
        arr.push_back(d);
    }
    resp = json{{"devices", arr}};
}

bool open_streams(int input_dev, int output_dev, int sr, std::string& err) {
    std::lock_guard<std::mutex> lk(core.state_mtx);
    if (core.stream) {
        err = "stream already open";
        return false;
    }
    PaStreamParameters inParams{}, outParams{};
    if (input_dev >= 0) {
        inParams.device = input_dev;
        inParams.channelCount = CHANNELS;
        inParams.sampleFormat = paFloat32;
        inParams.suggestedLatency = Pa_GetDeviceInfo(input_dev)->defaultLowInputLatency;
        inParams.hostApiSpecificStreamInfo = nullptr;
    }
    if (output_dev >= 0) {
        outParams.device = output_dev;
        outParams.channelCount = CHANNELS;
        outParams.sampleFormat = paFloat32;
        outParams.suggestedLatency = Pa_GetDeviceInfo(output_dev)->defaultLowOutputLatency;
        outParams.hostApiSpecificStreamInfo = nullptr;
    }

    PaError errpa = Pa_OpenStream(&core.stream,
                                 (input_dev>=0? &inParams: nullptr),
                                 (output_dev>=0? &outParams: nullptr),
                                 (double)sr,
                                 paFramesPerBufferUnspecified,
                                 paNoFlag,
                                 audioCallback,
                                 &core);
    if (errpa != paNoError) {
        err = Pa_GetErrorText(errpa);
        core.stream = nullptr;
        return false;
    }
    errpa = Pa_StartStream(core.stream);
    if (errpa != paNoError) {
        err = Pa_GetErrorText(errpa);
        Pa_CloseStream(core.stream);
        core.stream = nullptr;
        return false;
    }
    core.input_device = input_dev;
    core.output_device = output_dev;
    core.sample_rate = sr;
    core.running.store(true);
    core.ring.clear();
    return true;
}

void close_streams() {
    std::lock_guard<std::mutex> lk(core.state_mtx);
    if (core.stream) {
        Pa_StopStream(core.stream);
        Pa_CloseStream(core.stream);
        core.stream = nullptr;
    }
    core.running.store(false);
    core.ring.clear();
}

void worker_thread_fn() {
    std::vector<float> buffer(1024);
    while (true) {
        if (core.running.load()) {
            size_t got = core.ring.pop(buffer.data(), buffer.size());
            if (got>0) {
                double sum=0.0;
                for (size_t i=0;i<got;i++) sum += buffer[i]*buffer[i];
                double rms = sqrt(sum / (double)got);
                core.last_rx_rms = (float)rms;
            } else {
                std::this_thread::sleep_for(std::chrono::milliseconds(5));
            }
        } else {
            std::this_thread::sleep_for(std::chrono::milliseconds(50));
        }
    }
}

void handle_client(socket_t client_fd) {
    constexpr size_t BUF_SZ = 8192;
    std::vector<char> buf(BUF_SZ);
    std::string carry;
    while (true) {
        int r = recv(client_fd, buf.data(), (int)buf.size(), 0);
        if (r <= 0) break;
        carry.append(buf.data(), buf.data()+r);
        size_t pos;
        while ((pos = carry.find('\n')) != std::string::npos) {
            std::string line = carry.substr(0,pos);
            carry.erase(0,pos+1);
            if (line.size()==0) continue;
            json req;
            try {
                req = json::parse(line);
            } catch (...) {
                json resp = { {"ok", false}, {"error", "invalid json"} };
                std::string s = resp.dump() + "\n";
                send(client_fd, s.c_str(), (int)s.size(), 0);
                continue;
            }
            json resp;
            std::string cmd = req.value("cmd", "");
            if (cmd == "list_devices") {
                list_devices(resp);
                resp["ok"] = true;
            } else if (cmd == "open") {
                int in = req.value("input", -1);
                int out = req.value("output", -1);
                int sr = req.value("sr", DEFAULT_SR);
                std::string err;
                if (open_streams(in, out, sr, err)) {
                    resp = { {"ok", true} };
                } else {
                    resp = { {"ok", false}, {"error", err} };
                }
            } else if (cmd == "close") {
                close_streams();
                resp = { {"ok", true} };
            } else if (cmd == "status") {
                resp = {
                    {"ok", true},
                    {"running", core.running.load()},
                    {"last_rx_rms", core.last_rx_rms},
                    {"tx_enabled", core.tx_enabled.load()},
                    {"input_device", core.input_device},
                    {"output_device", core.output_device},
                    {"sample_rate", core.sample_rate}
                };
            } else if (cmd == "start_tx") {
                float f = req.value("tone_freq", 600.0f);
                core.tx_tone_freq.store(f);
                core.tx_enabled.store(true);
                resp = { {"ok", true} };
            } else if (cmd == "stop_tx") {
                core.tx_enabled.store(false);
                resp = { {"ok", true} };
            } else {
                resp = { {"ok", false}, {"error", "unknown command"} };
            }
            std::string s = resp.dump() + "\n";
            send(client_fd, s.c_str(), (int)s.size(), 0);
        }
    }
    CLOSE_SOCKET(client_fd);
}

void control_server_thread() {
#ifdef _WIN32
    // Winsock already started in main
#endif
    int listen_fd = (int)socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
    if (listen_fd < 0) {
        std::cerr << "control socket create failed\n";
        return;
    }
    int opt = 1;
#ifdef _WIN32
    setsockopt(listen_fd, SOL_SOCKET, SO_REUSEADDR, (const char*)&opt, sizeof(opt));
#else
    setsockopt(listen_fd, SOL_SOCKET, SO_REUSEADDR, &opt, sizeof(opt));
#endif
    sockaddr_in addr{};
    addr.sin_family = AF_INET;
    addr.sin_port = htons(CONTROL_PORT);
    addr.sin_addr.s_addr = inet_addr("127.0.0.1");
    if (bind(listen_fd, (struct sockaddr*)&addr, sizeof(addr)) != 0) {
        std::cerr << "bind failed on 127.0.0.1:" << CONTROL_PORT << "\n";
        CLOSE_SOCKET(listen_fd);
        return;
    }
    listen(listen_fd, 5);
    std::cout << "Control server listening on 127.0.0.1:" << CONTROL_PORT << "\n";
    while (true) {
        socket_t client = accept(listen_fd, nullptr, nullptr);
        if (client != INVALID_SOCKET) {
            std::thread t([client](){ handle_client(client); });
            t.detach();
        }
    }
}

int main() {
#ifdef _WIN32
    WSADATA wsaData;
    if (WSAStartup(MAKEWORD(2,2), &wsaData) != 0) {
        std::cerr << "WSAStartup failed\n";
        return 1;
    }
#endif

    PaError paerr = Pa_Initialize();
    if (paerr != paNoError) {
        std::cerr << "PortAudio init error: " << Pa_GetErrorText(paerr) << "\n";
        return 1;
    }

    std::thread worker(worker_thread_fn);
    std::thread ctrl(control_server_thread);

    std::cout << "Hush core skeleton running. Control port: " << CONTROL_PORT << "\n";
    std::cout << "Send JSON commands (newline delimited) to localhost:" << CONTROL_PORT << "\n";

    worker.join(); // never returns in this skeleton
    ctrl.join();

    Pa_Terminate();
#ifdef _WIN32
    WSACleanup();
#endif
    return 0;
}

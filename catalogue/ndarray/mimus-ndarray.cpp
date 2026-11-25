// mimus-ndarray: a reference server for what no EPICS base record produces.
//
// softIocPVX, with the catalogue's databases, covers every type QSRV2 serves
// from base records. Two kinds of channel are missing from it, and they are
// the ones that load a consumer by bytes rather than by count: NTNDArray
// images, as areaDetector serves them, and large waveforms. No base record
// produces an NTNDArray, and none fills a large array with changing values
// without C code. This program serves both, built with PVXS's own
// normative-type builders, so that MIMUS calibrates against the bytes PVXS
// sends for them, as it does against softIocPVX for the rest.
//
// Channels, two of each type, as calibration wants them:
//
//   MIM:IMG:0,  MIM:IMG:1    NTNDArray, mono, WIDTH x HEIGHT, 8 or 16 bits
//   MIM:WAVE:0, MIM:WAVE:1   NTScalarArray of LENGTH doubles
//
// Every channel posts RATE times a second: a new frame with its uniqueId,
// dataTimeStamp and timeStamp, or a new waveform with its timeStamp. Each
// post is a delta, as a detector driver's is: only what changed is marked.
// The pictures are a gradient that moves, with noise on it, so that frames
// differ and compress as camera frames do rather than as constant ones.
//
// Usage:
//   mimus-ndarray [--width 1024] [--height 1024] [--bits 8|16]
//                 [--length 16384] [--rate 10]
//
// It listens where PVXS servers do, as EPICS_PVAS_SERVER_PORT and the other
// EPICS_PVAS_* variables say; beside softIocPVX, give it another port.

#include <algorithm>
#include <atomic>
#include <chrono>
#include <cmath>
#include <csignal>
#include <cstdint>
#include <cstdlib>
#include <iostream>
#include <string>
#include <thread>
#include <vector>

#include <pvxs/data.h>
#include <pvxs/nt.h>
#include <pvxs/server.h>
#include <pvxs/sharedArray.h>
#include <pvxs/sharedpv.h>

namespace {

using system_time = std::chrono::system_clock::time_point;

std::atomic<bool> stop{false};

void on_signal(int) { stop = true; }

struct Options {
    uint32_t width = 1024;
    uint32_t height = 1024;
    uint32_t bits = 8;
    uint32_t length = 16384;
    double rate = 10.0;
};

void usage(const char *argv0) {
    std::cerr << "usage: " << argv0
              << " [--width N] [--height N] [--bits 8|16] [--length N] [--rate HZ]\n";
}

bool whole(const char *text, uint32_t &out) {
    char *end = nullptr;
    unsigned long v = std::strtoul(text, &end, 10);
    if (!end || *end != '\0' || v == 0 || v > 1000000)
        return false;
    out = uint32_t(v);
    return true;
}

bool parse(int argc, char **argv, Options &o) {
    for (int i = 1; i < argc; i++) {
        std::string a(argv[i]);
        if (a == "-h" || a == "--help") {
            usage(argv[0]);
            std::exit(0);
        }
        if (i + 1 >= argc)
            return false;
        const char *v = argv[++i];
        bool ok;
        if (a == "--width")
            ok = whole(v, o.width);
        else if (a == "--height")
            ok = whole(v, o.height);
        else if (a == "--bits")
            ok = whole(v, o.bits) && (o.bits == 8 || o.bits == 16);
        else if (a == "--length")
            ok = whole(v, o.length);
        else if (a == "--rate") {
            char *end = nullptr;
            o.rate = std::strtod(v, &end);
            ok = end && *end == '\0' && o.rate > 0 && std::isfinite(o.rate);
        } else
            ok = false;
        if (!ok)
            return false;
    }
    return true;
}

// A small, fast generator: the frames need to differ, not to be good noise.
uint32_t xorshift(uint32_t &s) {
    s ^= s << 13;
    s ^= s >> 17;
    s ^= s << 5;
    return s;
}

void stamp(pvxs::Value &v, const std::string &field, system_time at) {
    auto ns = std::chrono::duration_cast<std::chrono::nanoseconds>(at.time_since_epoch()).count();
    v[field + ".secondsPastEpoch"] = int64_t(ns / 1000000000);
    v[field + ".nanoseconds"] = int32_t(ns % 1000000000);
}

struct Image {
    pvxs::server::SharedPV pv;
    pvxs::Value prototype;
    int32_t unique;
    uint32_t seed;
};

struct Wave {
    pvxs::server::SharedPV pv;
    pvxs::Value prototype;
    uint32_t seed;
};

template <typename Pixel>
pvxs::shared_array<const void> pixels(Image &img, const Options &o, uint64_t n) {
    // The ramp spans the pixel's range across the frame and moves with each
    // frame; the noise is a few counts, as a sensor's is.
    const uint32_t top = (1u << o.bits) - 1;
    const uint32_t span = o.width + o.height;
    pvxs::shared_array<Pixel> out(size_t(o.width) * o.height);
    for (uint32_t y = 0; y < o.height; y++) {
        for (uint32_t x = 0; x < o.width; x++) {
            uint64_t ramp = (uint64_t(x + y) * top / span + n * (top / 64 + 1)) % (top + 1);
            uint32_t noise = xorshift(img.seed) & (o.bits == 8 ? 0x0F : 0xFF);
            out[size_t(y) * o.width + x] = Pixel(std::min<uint64_t>(top, ramp + noise));
        }
    }
    return out.freeze().template castTo<const void>();
}

pvxs::Value frame(Image &img, const Options &o, uint64_t n, system_time at) {
    auto v = img.prototype.cloneEmpty();
    if (o.bits == 8)
        v["value->ubyteValue"] = pixels<uint8_t>(img, o, n);
    else
        v["value->ushortValue"] = pixels<uint16_t>(img, o, n);
    v["uniqueId"] = ++img.unique;
    stamp(v, "dataTimeStamp", at);
    stamp(v, "timeStamp", at);
    return v;
}

pvxs::Value waveform(Wave &w, const Options &o, uint64_t n, system_time at) {
    const double pi = 3.14159265358979323846;
    pvxs::shared_array<double> values(o.length);
    for (uint32_t i = 0; i < o.length; i++) {
        double phase = 2.0 * pi * (double(i) / o.length + double(n) / 50.0);
        double noise = double(xorshift(w.seed) & 0xFFFF) / 65536.0 - 0.5;
        values[i] = 100.0 * std::sin(phase) + noise;
    }
    auto v = w.prototype.cloneEmpty();
    v["value"] = values.freeze();
    stamp(v, "timeStamp", at);
    return v;
}

// The parts of an image that do not change from frame to frame: its size,
// its dimensions, and the ColorMode attribute areaDetector names it with.
pvxs::Value image_prototype(const Options &o) {
    auto v = pvxs::nt::NTNDArray{}.create();
    const int64_t bytes = int64_t(o.width) * o.height * (o.bits / 8);
    v["codec.name"] = "";
    v["compressedSize"] = bytes;
    v["uncompressedSize"] = bytes;

    pvxs::shared_array<pvxs::Value> dimension(2);
    const uint32_t sizes[2] = {o.width, o.height};
    for (int d = 0; d < 2; d++) {
        dimension[d] = v["dimension"].allocMember();
        dimension[d]["size"] = int32_t(sizes[d]);
        dimension[d]["offset"] = int32_t(0);
        dimension[d]["fullSize"] = int32_t(sizes[d]);
        dimension[d]["binning"] = int32_t(1);
        dimension[d]["reverse"] = false;
    }
    v["dimension"] = dimension.freeze();

    pvxs::shared_array<pvxs::Value> attributes(1);
    attributes[0] = v["attribute"].allocMember();
    attributes[0]["name"] = "ColorMode";
    attributes[0]["value"] = int32_t(0); // NDColorModeMono
    attributes[0]["descriptor"] = "Color mode";
    attributes[0]["sourceType"] = int32_t(0);
    attributes[0]["source"] = "Driver";
    v["attribute"] = attributes.freeze();
    return v;
}

pvxs::Value wave_prototype() {
    auto v = pvxs::nt::NTScalar{pvxs::TypeCode::Float64A, true}.create();
    v["display.limitLow"] = -100.0;
    v["display.limitHigh"] = 100.0;
    v["display.units"] = "mm";
    return v;
}

// The opening value: the prototype, with a first delta applied.
pvxs::Value opening(const pvxs::Value &prototype, const pvxs::Value &first) {
    auto v = prototype.clone();
    v.assign(first);
    return v;
}

} // namespace

int main(int argc, char **argv) {
    Options o;
    if (!parse(argc, argv, o)) {
        usage(argv[0]);
        return 2;
    }

    auto server = pvxs::server::Server::fromEnv();
    std::vector<Image> images;
    std::vector<Wave> waves;
    const auto now = std::chrono::system_clock::now();

    for (uint32_t i = 0; i < 2; i++) {
        Image img{pvxs::server::SharedPV::buildReadonly(), image_prototype(o), 0,
                  0x9E3779B9u + i};
        img.pv.open(opening(img.prototype, frame(img, o, 0, now)));
        server.addPV("MIM:IMG:" + std::to_string(i), img.pv);
        images.push_back(std::move(img));

        Wave w{pvxs::server::SharedPV::buildReadonly(), wave_prototype(), 0x85EBCA6Bu + i};
        w.pv.open(opening(w.prototype, waveform(w, o, 0, now)));
        server.addPV("MIM:WAVE:" + std::to_string(i), w.pv);
        waves.push_back(std::move(w));
    }

    std::signal(SIGINT, on_signal);
    std::signal(SIGTERM, on_signal);
    server.start();
    std::cout << "mimus-ndarray: " << o.width << "x" << o.height << " " << o.bits
              << "-bit frames and " << o.length << "-element waveforms at " << o.rate
              << " Hz, on port " << server.config().tcp_port << std::endl;

    // Paced on the monotonic clock; stamped with the wall clock.
    const auto period = std::chrono::duration_cast<std::chrono::steady_clock::duration>(
        std::chrono::duration<double>(1.0 / o.rate));
    auto next = std::chrono::steady_clock::now();
    for (uint64_t n = 1; !stop; n++) {
        next += period;
        std::this_thread::sleep_until(next);
        const auto at = std::chrono::system_clock::now();
        for (auto &img : images)
            img.pv.post(frame(img, o, n, at));
        for (auto &w : waves)
            w.pv.post(waveform(w, o, n, at));
    }
    server.stop();
    return 0;
}

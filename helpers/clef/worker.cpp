// One immutable Clef model per owner. No listener, shell, download or chat API.
#include "common.h"
#include "ggml-backend.h"
#include "log.h"
#include "server-context.h"
#include <nlohmann/json.hpp>
extern "C" {
#include "hash/sha256/sha256.h"
}
#include <array>
#include <chrono>
#include <filesystem>
#include <fcntl.h>
#include <functional>
#include <iostream>
#include <set>
#include <stdexcept>
#include <sys/stat.h>
#include <thread>
#include <unistd.h>
#include <vector>

namespace {
using wire_json = nlohmann::ordered_json;
constexpr size_t MAX_FRAME = 1024 * 1024;
constexpr size_t MAX_QUESTIONS = 64;
constexpr size_t MAX_OPTIONS = 255;
using clock_type = std::chrono::steady_clock;

void require(bool valid) {
    if (!valid) throw std::runtime_error("invalid protocol");
}

bool frame(std::string & out) {
    out.clear();
    char c;
    while (std::cin.get(c)) {
        if (c == '\n') return true;
        require(out.size() < MAX_FRAME);
        out.push_back(c);
    }
    require(out.empty()); // A partial frame is not a complete request.
    return false;
}

wire_json parse(const std::string & text) {
    std::vector<std::set<std::string>> keys;
    bool duplicate = false;
    auto value = wire_json::parse(text, [&](int depth, wire_json::parse_event_t event, wire_json & item) {
        require(depth <= 32);
        if (event == wire_json::parse_event_t::object_start) keys.emplace_back();
        if (event == wire_json::parse_event_t::key) {
            require(!keys.empty());
            duplicate |= !keys.back().insert(item.get<std::string>()).second;
        }
        if (event == wire_json::parse_event_t::object_end) keys.pop_back();
        return true;
    });
    require(!duplicate && value.is_object());
    return value;
}

void fields(const wire_json & value, std::initializer_list<const char *> names) {
    require(value.is_object() && value.size() == names.size());
    for (const char * name : names) require(value.contains(name));
}

bool identifier(const wire_json & value) {
    if (!value.is_string()) return false;
    const auto text = value.get<std::string>();
    if (text.empty() || text.size() > 128) return false;
    for (unsigned char c : text) {
        if (!((c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') ||
              (c >= '0' && c <= '9') || c == '-' || c == '_' || c == '.')) return false;
    }
    return true;
}

bool label(const std::string & value) {
    return !value.empty() && value.size() <= 512;
}

void emit(const wire_json & value) {
    const auto text = value.dump();
    require(text.size() <= MAX_FRAME);
    std::cout << text << '\n' << std::flush;
    require(std::cout.good());
}

struct model_file {
    int fd = -1;
    FILE * stream = nullptr;
    struct stat original {};
    model_file(const std::string & path, uint64_t size, const std::string & expected) {
        require(path.size() <= 4096 && path.find('\0') == std::string::npos);
        const std::filesystem::path model(path);
        require(model.is_absolute() && std::filesystem::canonical(model) == model);
        fd = open(path.c_str(), O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
        try {
            require(fd >= 0 && fstat(fd, &original) == 0 && S_ISREG(original.st_mode) &&
                    original.st_nlink == 1 && original.st_size > 0 &&
                    static_cast<uint64_t>(original.st_size) == size);
            sha256_t hash;
            sha256_init(&hash);
            std::array<unsigned char, 1024 * 1024> buffer;
            ssize_t count;
            while ((count = read(fd, buffer.data(), buffer.size())) > 0) {
                sha256_update(&hash, buffer.data(), static_cast<size_t>(count));
            }
            require(count == 0 && lseek(fd, 0, SEEK_SET) == 0);
            std::array<unsigned char, SHA256_DIGEST_SIZE> digest;
            sha256_final(&hash, digest.data());
            const char * hex = "0123456789abcdef";
            std::string actual;
            for (unsigned char byte : digest) {
                actual.push_back(hex[byte >> 4]);
                actual.push_back(hex[byte & 15]);
            }
            require(actual == expected);
            stream = fdopen(fd, "rb");
            require(stream != nullptr);
        } catch (...) {
            if (stream) fclose(stream);
            else if (fd >= 0) close(fd);
            fd = -1;
            throw;
        }
    }
    model_file(const model_file &) = delete;
    model_file & operator=(const model_file &) = delete;
    ~model_file() { if (stream) fclose(stream); else if (fd >= 0) close(fd); }
    std::string path() const { return "/dev/fd/" + std::to_string(fd); }
    void unchanged() const {
        struct stat current {};
        require(fstat(fd, &current) == 0 && current.st_dev == original.st_dev &&
                current.st_ino == original.st_ino && current.st_size == original.st_size &&
                current.st_nlink == 1 && current.st_mtime == original.st_mtime &&
                current.st_ctime == original.st_ctime &&
                current.st_mtimespec.tv_nsec == original.st_mtimespec.tv_nsec &&
                current.st_ctimespec.tv_nsec == original.st_ctimespec.tv_nsec);
    }
};

void questions(const wire_json & asked) {
    require(asked.is_object() && !asked.empty() && asked.size() <= MAX_QUESTIONS);
    for (auto it = asked.begin(); it != asked.end(); ++it) {
        require(label(it.key()) && it.value().is_object());
        const auto & q = it.value();
        require(q.contains("type") && q["type"].is_string());
        require(q.contains("instructions") && q["instructions"].is_string());
        const auto instruction = q["instructions"].get<std::string>();
        require(!instruction.empty() && instruction.size() <= 16384);
        const auto type = q["type"].get<std::string>();
        if (type == "noul") {
            fields(q, {"type", "instructions"});
        } else {
            fields(q, {"type", "instructions", "criteria"});
            const auto & options = q["criteria"];
            require(!options.empty() && options.size() <= MAX_OPTIONS);
            require((type == "choice" && options.is_object()) || (type == "score" && options.is_array()));
            for (auto opt = options.begin(); opt != options.end(); ++opt) {
                if (type == "choice") require(label(opt.key()));
                require(opt.value().is_string() && opt.value().get<std::string>().size() <= 16384);
            }
        }
    }
}

void text_state(const wire_json & state) {
    // The upstream route can decode media in message-shaped states. This
    // worker is text-only: refuse those parts rather than stripping them.
    const auto & messages = state.is_object() && state.contains("messages") ? state["messages"] : state;
    if (!messages.is_array()) return;
    for (const auto & msg : messages) {
        if (!msg.is_object() || !msg.contains("content") || !msg["content"].is_array()) continue;
        for (const auto & part : msg["content"]) {
            if (!part.is_object() || !part.contains("type")) continue;
            require(part["type"] != "image_url" && part["type"] != "input_audio");
        }
    }
}

struct loop_guard {
    server_context & context;
    std::thread thread;
    explicit loop_guard(server_context & ctx) : context(ctx), thread([&ctx] { ctx.start_loop(); }) {}
    ~loop_guard() { context.terminate(); if (thread.joinable()) thread.join(); }
};

int run() {
    std::string text;
    require(frame(text));
    const auto init = parse(text);
    fields(init, {"protocol", "owner", "generation", "model", "model_bytes", "model_sha256",
                  "context_tokens", "device", "request_timeout_ms"});
    require(init["protocol"].is_number_unsigned() && init["protocol"] == 1 && identifier(init["owner"]) && identifier(init["generation"]));
    require(init["model"].is_string() && init["model_sha256"].is_string());
    const auto digest = init["model_sha256"].get<std::string>();
    require(digest.size() == 64 && digest.find_first_not_of("0123456789abcdef") == std::string::npos);
    require(init["model_bytes"].is_number_unsigned() && init["model_bytes"].get<uint64_t>() <= 64ULL * 1024 * 1024 * 1024);
    require(init["context_tokens"].is_number_unsigned() && init["request_timeout_ms"].is_number_unsigned());
    const auto tokens = init["context_tokens"].get<uint64_t>();
    const auto timeout = init["request_timeout_ms"].get<uint64_t>();
    require(tokens >= 1024 && tokens <= 16384 && timeout >= 1000 && timeout <= 120000);
    require(init["device"] == "cpu" || init["device"] == "metal");

    common_log_set_stdo(common_log_main(), false);
    llama_log_set([](ggml_log_level, const char *, void *) {}, nullptr);
    llama_backend_init();
    common_params params;
    params.offline = true;
    params.fit_params = false;
    params.warmup = false;
    params.ctx_shift = false;
    params.n_ctx = static_cast<int32_t>(tokens);
    params.n_batch = params.n_ubatch = static_cast<int32_t>(tokens);
    params.n_parallel = 1;
    params.n_ctx_checkpoints = 0;
    params.cpuparams.n_threads = 4;
    params.cpuparams_batch.n_threads = 4;
    params.no_kv_offload = params.no_op_offload = init["device"] == "cpu";
    params.n_gpu_layers = init["device"] == "cpu" ? 0 : -2;
    // Use the portable CPU paths. Optimized buffer placements may support
    // ordinary mat-muls but cannot be assumed to support the joint head.
    if (init["device"] == "cpu") {
        params.flash_attn_type = LLAMA_FLASH_ATTN_TYPE_DISABLED;
        params.no_extra_bufts = true;
    }
    if (init["device"] == "metal") {
        ggml_backend_dev_t selected = nullptr;
        for (size_t i = 0; i < ggml_backend_dev_count(); ++i) {
            auto dev = ggml_backend_dev_get(i);
            auto reg = ggml_backend_dev_backend_reg(dev);
            if (ggml_backend_dev_type(dev) == GGML_BACKEND_DEVICE_TYPE_GPU &&
                std::string(ggml_backend_reg_name(reg)) == "Metal") selected = dev;
        }
        require(selected != nullptr); // Never turn an explicit Metal selection into CPU.
        params.devices = {selected, nullptr};
    } else {
        params.devices = {nullptr};
    }
    model_file model(init["model"].get<std::string>(), init["model_bytes"].get<uint64_t>(), digest);
    params.model.path = model.path(); // Metadata only: never reopen a /dev/fd alias.
    params.model.file = model.stream; // Borrowed until context destruction; no pathname race.
    server_context context;
    server_routes routes(params, context);
    require(context.load_model(params));
    require(common_get_decision_type(llama_get_model(context.get_llama_context())) == COMMON_DECISION_TYPE_CLEF);
    model.unchanged();
    routes.update_meta(context);
    loop_guard running(context);
    const auto base = wire_json{{"protocol", 1}, {"owner", init["owner"]}, {"generation", init["generation"]}};
    auto ready = base;
    ready.update({{"status", "ready"}, {"runtime_revision", "620d2225cc436ab0b001e3200e590e3b286ef9ab+polaris-fileptr-v1"},
                  {"model_sha256", digest}, {"device", init["device"]}, {"context_tokens", tokens}});
    emit(ready);
    uint64_t last_id = 0;
    while (frame(text)) {
        const auto request = parse(text);
        require(request.contains("op"));
        const bool shutdown = request["op"] == "shutdown";
        if (shutdown) fields(request, {"protocol", "owner", "generation", "request_id", "op"});
        else fields(request, {"protocol", "owner", "generation", "request_id", "op", "state", "questions"});
        require(request["protocol"].is_number_unsigned() && request["protocol"] == 1 && request["owner"] == init["owner"] &&
                request["generation"] == init["generation"] && request["request_id"].is_number_unsigned());
        const auto id = request["request_id"].get<uint64_t>();
        require(id > last_id);
        last_id = id;
        auto reply = base;
        reply["request_id"] = id;
        if (shutdown) { reply["status"] = "closed"; emit(reply); break; }
        require(request["op"] == "decide");
        questions(request["questions"]);
        text_state(request["state"]);
        model.unchanged();
        const auto deadline = clock_type::now() + std::chrono::milliseconds(timeout);
        const std::function<bool()> cancelled = [&deadline] { return clock_type::now() >= deadline; };
        server_http_req req{{}, {}, "/v1/systemone", "",
                            wire_json{{"state", request["state"]}, {"questions", request["questions"]}}.dump(), {}, cancelled};
        const auto response = routes.post_systemone(req); // In-process route; no socket is created.
        if (cancelled()) {
            reply["status"] = "outcome_unknown";
        } else if (response->status == 200 && !response->is_stream()) {
            reply["status"] = "ok";
            reply["decision"] = parse(response->data);
        } else {
            reply["status"] = "refused";
            reply["code"] = response->status;
        }
        response->on_complete();
        model.unchanged();
        emit(reply);
        if (cancelled()) break; // No subsequent request reuses a timed-out execution.
    }
    return 0;
}
} // namespace

int main(int argc, char **) {
    try {
        require(argc == 1);
        return run();
    } catch (...) {
        // Never echo model paths, requests, upstream errors or environment values.
        std::cout << "{\"protocol\":1,\"status\":\"invalid_or_unavailable\"}\n" << std::flush;
        return 2;
    }
}

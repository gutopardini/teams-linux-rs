// Portal screen frames as CPU BGRA. No DMA-BUF modifiers: on Intel Arc an EGL
// import can succeed with black pixels and libwebrtc then never falls back.

#include <pipewire/pipewire.h>
#include <spa/param/video/format-utils.h>
#include <spa/param/video/raw.h>

#include <cstdint>
#include <unistd.h>

extern "C" {

struct teams_cpu_frame {
  uint32_t width;
  uint32_t height;
  uint32_t stride;
  uint32_t data_type;
  uint32_t format;
  const uint8_t* data;
  size_t size;
};

typedef void (*teams_cpu_frame_fn)(void* user, const struct teams_cpu_frame* frame);

int teams_cpu_screencast(int fd, uint32_t node_id, const uint8_t* stop, teams_cpu_frame_fn callback,
                         void* user);

}  // extern "C"

namespace {

struct State {
  pw_stream* stream = nullptr;
  teams_cpu_frame_fn callback = nullptr;
  void* user = nullptr;
  const uint8_t* stop = nullptr;
  uint32_t width = 0;
  uint32_t height = 0;
  uint32_t format = SPA_VIDEO_FORMAT_UNKNOWN;
  int failed = 0;
  bool buffers_set = false;
};

struct Pipe {
  pw_thread_loop* loop = nullptr;
  pw_context* context = nullptr;
  pw_core* core = nullptr;
  spa_hook hook{};
  pw_stream* stream = nullptr;
  bool started = false;

  void stop_stream() {
    if (started && loop != nullptr) {
      pw_thread_loop_stop(loop);
      started = false;
    }
    if (loop != nullptr && stream != nullptr) {
      pw_thread_loop_lock(loop);
      pw_stream_destroy(stream);
      stream = nullptr;
      pw_thread_loop_unlock(loop);
    }
  }

  ~Pipe() {
    stop_stream();
    if (loop != nullptr) {
      pw_thread_loop_lock(loop);
    }
    if (core != nullptr) {
      pw_core_disconnect(core);
    }
    if (context != nullptr) {
      pw_context_destroy(context);
    }
    if (loop != nullptr) {
      pw_thread_loop_unlock(loop);
      pw_thread_loop_destroy(loop);
    }
  }
};

void on_state(void* data, enum pw_stream_state /*old*/, enum pw_stream_state state,
              const char* /*error*/) {
  if (state == PW_STREAM_STATE_ERROR) {
    static_cast<State*>(data)->failed = 1;
  }
}

void on_param(void* data, uint32_t id, const struct spa_pod* param) {
  auto* state = static_cast<State*>(data);
  if (id != SPA_PARAM_Format || param == nullptr) {
    return;
  }
  spa_video_info_raw info{};
  if (spa_format_video_raw_parse(param, &info) < 0) {
    state->failed = 1;
    return;
  }
  state->width = info.size.width;
  state->height = info.size.height;
  state->format = info.format;
  if (state->buffers_set || state->stream == nullptr) {
    return;
  }
  state->buffers_set = true;
  uint8_t buffer[512];
  spa_pod_builder builder = SPA_POD_BUILDER_INIT(buffer, sizeof(buffer));
  const int mem = (1 << SPA_DATA_MemFd) | (1 << SPA_DATA_MemPtr);
  const spa_pod* params[1];
  params[0] = reinterpret_cast<const spa_pod*>(spa_pod_builder_add_object(
      &builder, SPA_TYPE_OBJECT_ParamBuffers, SPA_PARAM_Buffers, SPA_PARAM_BUFFERS_buffers,
      SPA_POD_CHOICE_RANGE_Int(8, 2, 16), SPA_PARAM_BUFFERS_blocks, SPA_POD_Int(1),
      SPA_PARAM_BUFFERS_dataType, SPA_POD_CHOICE_FLAGS_Int(mem)));
  if (params[0] == nullptr || pw_stream_update_params(state->stream, params, 1) < 0) {
    state->failed = 1;
  }
}

void on_process(void* data) {
  auto* state = static_cast<State*>(data);
  if (state->stop != nullptr && __atomic_load_n(state->stop, __ATOMIC_RELAXED)) {
    return;
  }
  pw_buffer* buffer = pw_stream_dequeue_buffer(state->stream);
  if (buffer == nullptr) {
    return;
  }
  if (buffer->buffer == nullptr || buffer->buffer->n_datas < 1) {
    pw_stream_queue_buffer(state->stream, buffer);
    return;
  }
  spa_data* bytes = &buffer->buffer->datas[0];
  spa_chunk* chunk = bytes->chunk;
  uint32_t stride = state->width * 4;
  uint32_t size = 0;
  const uint8_t* pixels = nullptr;
  if (chunk != nullptr && bytes->data != nullptr) {
    if (chunk->stride > 0) {
      stride = static_cast<uint32_t>(chunk->stride);
    }
    if (chunk->offset <= chunk->size) {
      size = chunk->size - chunk->offset;
      pixels = static_cast<const uint8_t*>(bytes->data) + chunk->offset;
    }
  }
  if (state->callback != nullptr && pixels != nullptr && state->width >= 2 && state->height >= 2) {
    teams_cpu_frame frame{state->width, state->height, stride, bytes->type,
                          state->format, pixels, size};
    state->callback(state->user, &frame);
  }
  pw_stream_queue_buffer(state->stream, buffer);
}

const spa_pod* video_format(spa_pod_builder* builder, uint32_t format) {
  spa_rectangle def = SPA_RECTANGLE(320, 240);
  spa_rectangle min = SPA_RECTANGLE(1, 1);
  spa_rectangle max = SPA_RECTANGLE(8192, 8192);
  spa_fraction def_rate = SPA_FRACTION(15, 1);
  spa_fraction min_rate = SPA_FRACTION(0, 1);
  spa_fraction max_rate = SPA_FRACTION(60, 1);
  return reinterpret_cast<const spa_pod*>(spa_pod_builder_add_object(
      builder, SPA_TYPE_OBJECT_Format, SPA_PARAM_EnumFormat, SPA_FORMAT_mediaType,
      SPA_POD_Id(SPA_MEDIA_TYPE_video), SPA_FORMAT_mediaSubtype, SPA_POD_Id(SPA_MEDIA_SUBTYPE_raw),
      SPA_FORMAT_VIDEO_format, SPA_POD_Id(format), SPA_FORMAT_VIDEO_size,
      SPA_POD_CHOICE_RANGE_Rectangle(&def, &min, &max), SPA_FORMAT_VIDEO_framerate,
      SPA_POD_CHOICE_RANGE_Fraction(&def_rate, &min_rate, &max_rate)));
}

int run(int fd, uint32_t node_id, const uint8_t* stop, teams_cpu_frame_fn callback, void* user) {
  Pipe pipe;
  pipe.loop = pw_thread_loop_new("teams-screen", nullptr);
  if (pipe.loop == nullptr) {
    close(fd);
    return 1;
  }
  pw_thread_loop_lock(pipe.loop);
  pipe.context = pw_context_new(pw_thread_loop_get_loop(pipe.loop), nullptr, 0);
  if (pipe.context == nullptr) {
    pw_thread_loop_unlock(pipe.loop);
    close(fd);
    return 1;
  }
  pipe.core = pw_context_connect_fd(pipe.context, fd, nullptr, 0);
  if (pipe.core == nullptr) {
    pw_thread_loop_unlock(pipe.loop);
    return 1;
  }
  pw_properties* props = pw_properties_new(PW_KEY_MEDIA_TYPE, "Video", PW_KEY_MEDIA_CATEGORY,
                                           "Capture", PW_KEY_MEDIA_ROLE, "Screen", nullptr);
  pipe.stream = pw_stream_new(pipe.core, "teams-screen", props);
  if (pipe.stream == nullptr) {
    pw_thread_loop_unlock(pipe.loop);
    return 1;
  }
  State state;
  state.stream = pipe.stream;
  state.callback = callback;
  state.user = user;
  state.stop = stop;
  static const pw_stream_events events = {
      .version = PW_VERSION_STREAM_EVENTS,
      .state_changed = on_state,
      .param_changed = on_param,
      .process = on_process,
  };
  pw_stream_add_listener(pipe.stream, &pipe.hook, &events, &state);
  uint8_t buffer[2048];
  spa_pod_builder builder = SPA_POD_BUILDER_INIT(buffer, sizeof(buffer));
  const spa_pod* params[] = {
      video_format(&builder, SPA_VIDEO_FORMAT_BGRA),
      video_format(&builder, SPA_VIDEO_FORMAT_BGRx),
  };
  if (params[0] == nullptr || params[1] == nullptr ||
      pw_stream_connect(pipe.stream, PW_DIRECTION_INPUT, node_id,
                        static_cast<pw_stream_flags>(PW_STREAM_FLAG_AUTOCONNECT |
                                                     PW_STREAM_FLAG_MAP_BUFFERS),
                        params, 2) < 0) {
    pw_thread_loop_unlock(pipe.loop);
    return 1;
  }
  pw_thread_loop_unlock(pipe.loop);
  if (pw_thread_loop_start(pipe.loop) < 0) {
    return 1;
  }
  pipe.started = true;
  while (!__atomic_load_n(stop, __ATOMIC_RELAXED) && state.failed == 0) {
    usleep(20000);
  }
  const int failed = state.failed;
  pipe.stop_stream();
  return failed == 0 ? 0 : 1;
}

}  // namespace

extern "C" int teams_cpu_screencast(int fd, uint32_t node_id, const uint8_t* stop,
                                    teams_cpu_frame_fn callback, void* user) {
  if (fd < 0 || stop == nullptr || callback == nullptr) {
    if (fd >= 0) {
      close(fd);
    }
    return 1;
  }
  pw_init(nullptr, nullptr);
  const int rc = run(fd, node_id, stop, callback, user);
  pw_deinit();
  return rc;
}

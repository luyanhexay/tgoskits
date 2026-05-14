#ifndef _POSIX_C_SOURCE
#define _POSIX_C_SOURCE 199309L
#endif
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <unistd.h>
#include <pthread.h>
#include <time.h>
#include <errno.h>
#include <sys/stat.h>

#include <libuvc/libuvc.h>

#define UVC_FRAME_FORMAT_ANY 0
#define UVC_FRAME_FORMAT_YUYV 3
#define UVC_FRAME_FORMAT_MJPEG 7

#define MAX_PATH 512
#define MAX_SAVED_DEFAULT 1000000

typedef enum {
    FORMAT_ANY,
    FORMAT_MJPEG,
    FORMAT_YUYV
} frame_format_t;

typedef struct {
    frame_format_t format;
    frame_format_t probe_format;
    uint8_t format_index;
    uint8_t frame_index;
    int width;
    int height;
    int fps;
    uint32_t interval;
    uint32_t max_bit_rate;
    uint32_t max_frame_size;
    uint64_t estimated_bytes_per_sec;
} video_mode_t;

typedef struct {
    char dir[MAX_PATH];
    uint64_t every;
    uint64_t max_saved;
    int last_only;
} save_config_t;

typedef struct {
    uint64_t frames;
    uint64_t bytes;
    uint64_t saved;
    uint64_t save_errors;
    save_config_t *save;
    pthread_mutex_t last_frame_mutex;
    struct {
        uint64_t frame_id;
        uint32_t sequence;
        uint32_t width;
        uint32_t height;
        int frame_format;
        uint8_t *data;
        size_t data_len;
        size_t data_cap;
    } last_frame;
} frame_counters_t;

typedef struct {
    int device_index;
    frame_format_t format;
    int width;
    int height;
    int fps;
    int auto_min_data;
    int list_modes;
    int interval_sec;
    int duration_sec;
    uint64_t max_frames;
    save_config_t save;
} options_t;

static const char* format_str(frame_format_t fmt) {
    switch (fmt) {
        case FORMAT_MJPEG: return "mjpeg";
        case FORMAT_YUYV: return "yuyv";
        default: return "any";
    }
}

static int format_to_uvc(frame_format_t fmt) {
    switch (fmt) {
        case FORMAT_MJPEG: return UVC_FRAME_FORMAT_MJPEG;
        case FORMAT_YUYV: return UVC_FRAME_FORMAT_YUYV;
        default: return UVC_FRAME_FORMAT_ANY;
    }
}

static frame_format_t parse_format(const char *value) {
    if (strcasecmp(value, "mjpeg") == 0 || strcasecmp(value, "mjpg") == 0)
        return FORMAT_MJPEG;
    if (strcasecmp(value, "yuyv") == 0 || strcasecmp(value, "yuv") == 0)
        return FORMAT_YUYV;
    return FORMAT_ANY;
}

static int format_matches(frame_format_t requested, frame_format_t actual) {
    if (requested == FORMAT_ANY) return 1;
    if (requested == FORMAT_MJPEG) return actual == FORMAT_MJPEG;
    if (requested == FORMAT_YUYV) return actual == FORMAT_YUYV || actual == FORMAT_ANY;
    return 0;
}

static int format_rank(frame_format_t fmt) {
    switch (fmt) {
        case FORMAT_MJPEG: return 0;
        case FORMAT_YUYV: return 1;
        default: return 2;
    }
}

static uint32_t interval_to_fps(uint32_t interval) {
    if (interval == 0) return 0;
    return (uint32_t)((10000000u + interval / 2) / interval);
}

static uint64_t estimate_bytes_per_sec(const uvc_frame_desc_t *frame, int fps) {
    uint64_t by_bit_rate = (frame->dwMaxBitRate + 7) / 8;
    uint64_t by_frame_size = (uint64_t)(frame->dwMaxVideoFrameBufferSize > 0 ?
                                        frame->dwMaxVideoFrameBufferSize : 1) * (fps > 0 ? fps : 1);
    if (by_bit_rate == 0) return by_frame_size;
    if (by_frame_size == 0) return by_bit_rate;
    return by_bit_rate > by_frame_size ? by_bit_rate : by_frame_size;
}

static int is_yuyv_guid(const uint8_t guid[16]) {
    return memcmp(guid, "YUY2", 4) == 0;
}

static frame_format_t descriptor_frame_format(const uvc_format_desc_t *format) {
    switch (format->bDescriptorSubtype) {
        case UVC_VS_FORMAT_MJPEG: return FORMAT_MJPEG;
        case UVC_VS_FORMAT_UNCOMPRESSED:
            if (is_yuyv_guid(format->guidFormat)) return FORMAT_YUYV;
            return FORMAT_ANY;
        default: return FORMAT_ANY;
    }
}

static frame_format_t probe_frame_format(const uvc_format_desc_t *format) {
    switch (format->bDescriptorSubtype) {
        case UVC_VS_FORMAT_MJPEG: return FORMAT_MJPEG;
        case UVC_VS_FORMAT_UNCOMPRESSED: return FORMAT_YUYV;
        default: return FORMAT_ANY;
    }
}

static void print_help(const char *prog) {
    printf("uvc-fps\n\n"
           "USAGE:\n  %s [OPTIONS]\n\n"
           "OPTIONS:\n"
           "  --device <INDEX>        Zero-based UVC device index [default: 0]\n"
           "  --format <FORMAT>       any, mjpeg, or yuyv [default: mjpeg]\n"
           "  --width <PIXELS>        Frame width [default: 640]\n"
           "  --height <PIXELS>       Frame height [default: 480]\n"
           "  --fps <FPS>             Requested frame rate [default: 30]\n"
           "  --auto-min-data         Select the lowest-data probeable mode matching --format\n"
           "  --list-modes            Print UVC descriptor modes and exit\n"
           "  --interval-sec <SECS>   Reporting interval [default: 1]\n"
           "  --duration-sec <SECS>   Stop after this many seconds\n"
           "  --max-frames <N>        Stop after at least N frames\n"
           "  --save-dir <DIR>        Save frames to DIR with incrementing file names\n"
           "  --save-last             Cache frames while streaming, then save only the final frame\n"
           "  --save-every <N>        Save every Nth frame when --save-dir is set [default: 1]\n"
           "  --max-saved <N>         Stop saving after N saved frames\n"
           "  -h, --help              Print help\n",
           prog);
}

static void save_last_frame(frame_counters_t *counters, save_config_t *save) {
    pthread_mutex_lock(&counters->last_frame_mutex);

    if (counters->last_frame.data == NULL || counters->last_frame.data_len == 0) {
        pthread_mutex_unlock(&counters->last_frame_mutex);
        printf("uvc-fps: no frame captured; cannot save final image\n");
        return;
    }

    if (save->max_saved > 0 && counters->saved >= save->max_saved) {
        pthread_mutex_unlock(&counters->last_frame_mutex);
        return;
    }

    uint64_t saved_id = ++counters->saved;

    const char *ext = (counters->last_frame.frame_format == UVC_FRAME_FORMAT_MJPEG) ? "jpg" : "raw";
    char path[MAX_PATH];
    snprintf(path, sizeof(path), "%s/frame-%06lu.%s", save->dir, saved_id, ext);

    FILE *f = fopen(path, "wb");
    if (f) {
        size_t written = fwrite(counters->last_frame.data, 1, counters->last_frame.data_len, f);
        fclose(f);
        printf("uvc-fps: final frame saved id=%lu path=%s bytes=%zu frame_id=%lu sequence=%u size=%ux%u\n",
               saved_id, path, written, counters->last_frame.frame_id,
               counters->last_frame.sequence, counters->last_frame.width, counters->last_frame.height);
    } else {
        counters->save_errors++;
        fprintf(stderr, "uvc-fps: save error: %s: %s\n", path, strerror(errno));
    }

    pthread_mutex_unlock(&counters->last_frame_mutex);
}

static void save_frame(frame_counters_t *counters, save_config_t *save,
                       const uvc_frame_t *frame, uint64_t frame_id) {
    if (frame_id % save->every != 0) return;
    if (save->max_saved > 0 && counters->saved >= save->max_saved) return;
    if (frame->data == NULL || frame->data_bytes == 0) return;

    uint64_t saved_id = ++counters->saved;

    const char *ext = (frame->frame_format == UVC_FRAME_FORMAT_MJPEG) ? "jpg" : "raw";
    char path[MAX_PATH];
    snprintf(path, sizeof(path), "%s/frame-%06lu.%s", save->dir, saved_id, ext);

    FILE *f = fopen(path, "wb");
    if (f) {
        size_t written = fwrite(frame->data, 1, frame->data_bytes, f);
        fclose(f);
        printf("uvc-fps: saved id=%lu path=%s bytes=%zu\n",
               saved_id, path, written);
    } else {
        counters->save_errors++;
        if (counters->save_errors <= 5) {
            fprintf(stderr, "uvc-fps: save error: %s: %s\n", path, strerror(errno));
        }
    }
}

static void cache_last_frame(frame_counters_t *counters, const uvc_frame_t *frame, uint64_t frame_id) {
    if (frame->data == NULL || frame->data_bytes == 0) return;

    pthread_mutex_lock(&counters->last_frame_mutex);

    size_t needed = frame->data_bytes;
    if (counters->last_frame.data_cap < needed) {
        free(counters->last_frame.data);
        counters->last_frame.data = malloc(needed);
        counters->last_frame.data_cap = (counters->last_frame.data != NULL) ? needed : 0;
    }

    if (counters->last_frame.data != NULL) {
        memcpy(counters->last_frame.data, frame->data, needed);
        counters->last_frame.data_len = needed;
        counters->last_frame.frame_id = frame_id;
        counters->last_frame.sequence = frame->sequence;
        counters->last_frame.width = frame->width;
        counters->last_frame.height = frame->height;
        counters->last_frame.frame_format = frame->frame_format;
    } else {
        counters->save_errors++;
    }

    pthread_mutex_unlock(&counters->last_frame_mutex);
}

static void frame_callback(uvc_frame_t *frame, void *user_ptr) {
    frame_counters_t *counters = (frame_counters_t *)user_ptr;
    uint64_t frame_id = ++counters->frames;
    counters->bytes += frame->data_bytes;

    if (counters->save != NULL) {
        if (counters->save->last_only) {
            cache_last_frame(counters, frame, frame_id);
        } else {
            save_frame(counters, counters->save, frame, frame_id);
        }
    }
}

static int compare_modes(const void *a, const void *b) {
    const video_mode_t *ma = (const video_mode_t *)a;
    const video_mode_t *mb = (const video_mode_t *)b;

    // Sort by: estimated_bytes_per_sec, format_rank, resolution, fps, width, indices
    if (ma->estimated_bytes_per_sec != mb->estimated_bytes_per_sec)
        return (ma->estimated_bytes_per_sec > mb->estimated_bytes_per_sec) ? 1 : -1;
    int ra = format_rank(ma->format);
    int rb = format_rank(mb->format);
    if (ra != rb) return ra - rb;
    int ra_res = ma->width * ma->height;
    int rb_res = mb->width * mb->height;
    if (ra_res != rb_res) return ra_res - rb_res;
    if (ma->fps != mb->fps) return ma->fps - mb->fps;
    if (ma->width != mb->width) return ma->width - mb->width;
    if (ma->format_index != mb->format_index) return ma->format_index - mb->format_index;
    return ma->frame_index - mb->frame_index;
}

static int enumerate_modes(uvc_device_handle_t *devh, video_mode_t **modes_out, size_t *count_out) {
    video_mode_t *modes = NULL;
    size_t capacity = 0;
    size_t count = 0;

    const uvc_format_desc_t *format_desc = uvc_get_format_descs(devh);
    while (format_desc != NULL) {
        frame_format_t frame_format = descriptor_frame_format(format_desc);
        const uvc_frame_desc_t *frame_desc = format_desc->frame_descs;

        while (frame_desc != NULL) {
            // Get intervals
            uint32_t *intervals = NULL;
            size_t interval_count = 0;

            if (frame_desc->intervals) {
                for (int i = 0; i < frame_desc->bFrameIntervalType; i++) {
                    if (frame_desc->intervals[i] != 0) interval_count++;
                }
                intervals = malloc(interval_count * sizeof(uint32_t));
                for (size_t i = 0, j = 0; i < frame_desc->bFrameIntervalType && j < interval_count; i++) {
                    if (frame_desc->intervals[i] != 0) {
                        intervals[j++] = frame_desc->intervals[i];
                    }
                }
            } else {
                // Continuous frame interval
                if (frame_desc->dwMinFrameInterval == 0 || frame_desc->dwMaxFrameInterval == 0 ||
                    frame_desc->dwFrameIntervalStep == 0) {
                    interval_count = 1;
                    intervals = malloc(sizeof(uint32_t));
                    intervals[0] = frame_desc->dwDefaultFrameInterval;
                } else {
                    uint32_t min = frame_desc->dwMinFrameInterval;
                    uint32_t max = frame_desc->dwMaxFrameInterval;
                    uint32_t step = frame_desc->dwFrameIntervalStep;
                    interval_count = 0;
                    for (uint32_t iv = min; iv <= max && interval_count < 64; iv += step) {
                        interval_count++;
                    }
                    intervals = malloc(interval_count * sizeof(uint32_t));
                    for (size_t i = 0; i < interval_count; i++) {
                        intervals[i] = min + i * step;
                    }
                }
            }

            for (size_t i = 0; i < interval_count; i++) {
                uint32_t interval = intervals[i];
                if (interval == 0) continue;
                int fps = interval_to_fps(interval);
                if (fps <= 0) continue;

                // Expand array if needed
                if (count >= capacity) {
                    capacity = (capacity == 0) ? 256 : capacity * 2;
                    video_mode_t *new_modes = realloc(modes, capacity * sizeof(video_mode_t));
                    if (!new_modes) {
                        free(modes);
                        free(intervals);
                        return -1;
                    }
                    modes = new_modes;
                }

                modes[count].format = frame_format;
                modes[count].probe_format = probe_frame_format(format_desc);
                modes[count].format_index = format_desc->bFormatIndex;
                modes[count].frame_index = frame_desc->bFrameIndex;
                modes[count].width = frame_desc->wWidth;
                modes[count].height = frame_desc->wHeight;
                modes[count].fps = fps;
                modes[count].interval = interval;
                modes[count].max_bit_rate = frame_desc->dwMaxBitRate;
                modes[count].max_frame_size = frame_desc->dwMaxVideoFrameBufferSize;
                modes[count].estimated_bytes_per_sec = estimate_bytes_per_sec(frame_desc, fps);
                count++;
            }

            free(intervals);
            frame_desc = frame_desc->next;
        }
        format_desc = format_desc->next;
    }

    if (count == 0) {
        free(modes);
        return -1;
    }

    *modes_out = modes;
    *count_out = count;
    return 0;
}

static void print_modes(const video_mode_t *modes, size_t count) {
    printf("uvc-fps: modes count=%zu\n", count);
    for (size_t i = 0; i < count; i++) {
        const video_mode_t *m = &modes[i];
        printf("uvc-fps: mode format=%s size=%dx%d fps=%d interval=%u "
               "estimated_bytes_per_sec=%lu bitrate=%u max_frame=%u "
               "format_index=%u frame_index=%u\n",
               format_str(m->format), m->width, m->height, m->fps, m->interval,
               m->estimated_bytes_per_sec, m->max_bit_rate, m->max_frame_size,
               m->format_index, m->frame_index);
    }
}

static void get_monotonic_time(struct timespec *ts) {
    clock_gettime(CLOCK_MONOTONIC, ts);
}

static double timespec_diff_sec(const struct timespec *start, const struct timespec *end) {
    return (end->tv_sec - start->tv_sec) + (end->tv_nsec - start->tv_nsec) / 1e9;
}

static int ms_elapsed(const struct timespec *start) {
    struct timespec now;
    get_monotonic_time(&now);
    return (int)((now.tv_sec - start->tv_sec) * 1000 +
                 (now.tv_nsec - start->tv_nsec) / 1000000);
}

static int probe_exact_mode(uvc_device_handle_t *devh, frame_format_t format,
                            int width, int height, int fps, uvc_stream_ctrl_t *ctrl_out) {
    uvc_stream_ctrl_t ctrl;
    int uvc_format = format_to_uvc(format);

    printf("uvc-fps: entering uvc_get_stream_ctrl_format_size format=%s size=%dx%d fps=%d\n",
           format_str(format), width, height, fps);

    struct timespec start;
    get_monotonic_time(&start);

    int result = uvc_get_stream_ctrl_format_size(devh, &ctrl, uvc_format, width, height, fps);

    printf("uvc-fps: uvc_get_stream_ctrl_format_size returned elapsed_ms=%d format_index=%u "
           "frame_index=%u interval=%u max_frame=%u max_payload=%u iface=%u\n",
           ms_elapsed(&start), ctrl.bFormatIndex, ctrl.bFrameIndex,
           ctrl.dwFrameInterval, ctrl.dwMaxVideoFrameSize,
           ctrl.dwMaxPayloadTransferSize, ctrl.bInterfaceNumber);

    if (result < 0) {
        fprintf(stderr, "uvc-fps: uvc_get_stream_ctrl_format_size failed: %s\n", uvc_strerror(result));
        return -1;
    }

    *ctrl_out = ctrl;
    return 0;
}

static int control_matches_mode(const uvc_stream_ctrl_t *ctrl, const video_mode_t *mode) {
    return ctrl->bFormatIndex == mode->format_index &&
           ctrl->bFrameIndex == mode->frame_index &&
           ctrl->dwFrameInterval == mode->interval;
}

static int select_min_data_mode(uvc_device_handle_t *devh, const options_t *options,
                                 const video_mode_t *modes, size_t mode_count,
                                 uvc_stream_ctrl_t *ctrl_out, video_mode_t *selected_out) {
    // Create filtered list
    video_mode_t *candidates = malloc(mode_count * sizeof(video_mode_t));
    size_t candidate_count = 0;

    for (size_t i = 0; i < mode_count; i++) {
        if (format_matches(options->format, modes[i].format)) {
            candidates[candidate_count++] = modes[i];
        }
    }

    if (candidate_count == 0) {
        free(candidates);
        fprintf(stderr, "uvc-fps: no UVC modes match requested format %s\n", format_str(options->format));
        return -1;
    }

    // Sort candidates
    qsort(candidates, candidate_count, sizeof(video_mode_t), compare_modes);

    // Try each candidate
    for (size_t i = 0; i < candidate_count; i++) {
        const video_mode_t *mode = &candidates[i];
        uvc_stream_ctrl_t ctrl;

        int result = probe_exact_mode(devh, mode->probe_format, mode->width, mode->height, mode->fps, &ctrl);

        if (result == 0) {
            if (control_matches_mode(&ctrl, mode)) {
                *ctrl_out = ctrl;
                *selected_out = *mode;
                free(candidates);
                return 0;
            } else {
                printf("uvc-fps: mode rejected format=%s size=%dx%d fps=%d "
                       "reason=negotiated-mismatch expected_format_index=%u expected_frame_index=%u "
                       "expected_interval=%u actual_format_index=%u actual_frame_index=%u "
                       "actual_interval=%u actual_max_frame=%u actual_max_payload=%u\n",
                       format_str(mode->format), mode->width, mode->height, mode->fps,
                       mode->format_index, mode->frame_index, mode->interval,
                       ctrl.bFormatIndex, ctrl.bFrameIndex, ctrl.dwFrameInterval,
                       ctrl.dwMaxVideoFrameSize, ctrl.dwMaxPayloadTransferSize);
            }
        } else {
            printf("uvc-fps: mode rejected format=%s size=%dx%d fps=%d error=%s\n",
                   format_str(mode->format), mode->width, mode->height, mode->fps, uvc_strerror(result));
        }
    }

    free(candidates);
    fprintf(stderr, "uvc-fps: all auto-selected UVC modes failed probe\n");
    return -1;
}

static int report_loop(const options_t *options, frame_counters_t *counters) {
    struct timespec started, last, now;
    get_monotonic_time(&started);
    last = started;

    uint64_t last_frames = 0;
    uint64_t last_bytes = 0;

    while (1) {
        int next_sleep_ms;
        if (options->duration_sec > 0) {
            int remaining_ms = options->duration_sec * 1000 - ms_elapsed(&started);
            int interval_ms = options->interval_sec * 1000;
            next_sleep_ms = (remaining_ms < interval_ms) ? remaining_ms : interval_ms;
            if (next_sleep_ms < 0) next_sleep_ms = 0;
        } else {
            next_sleep_ms = options->interval_sec * 1000;
        }

        if (next_sleep_ms == 0) break;

        usleep(next_sleep_ms * 1000);

        get_monotonic_time(&now);
        uint64_t frames = counters->frames;
        uint64_t bytes = counters->bytes;
        uint64_t saved = counters->saved;
        uint64_t save_errors = counters->save_errors;

        double elapsed = timespec_diff_sec(&started, &now);
        double interval = timespec_diff_sec(&last, &now);
        uint64_t frame_delta = frames - last_frames;
        uint64_t byte_delta = bytes - last_bytes;

        double fps = frame_delta / (interval > 0 ? interval : 1e-9);
        double throughput_mib_s = byte_delta / (interval > 0 ? interval : 1e-9) / 1024.0 / 1024.0;

        printf("uvc-fps: frames=%lu fps=%.2f bytes=%lu saved=%lu save_errors=%lu "
               "throughput_mib_s=%.2f elapsed_sec=%.1f\n",
               frames, fps, bytes, saved, save_errors, throughput_mib_s, elapsed);

        if ((options->max_frames > 0 && frames >= options->max_frames) ||
            (options->duration_sec > 0 && elapsed >= options->duration_sec)) {
            break;
        }

        last = now;
        last_frames = frames;
        last_bytes = bytes;
    }

    struct timespec end;
    get_monotonic_time(&end);
    double elapsed = timespec_diff_sec(&started, &end);

    printf("uvc-fps: done duration_sec=%.1f frames=%lu avg_fps=%.2f bytes=%lu saved=%lu "
           "save_errors=%lu avg_throughput_mib_s=%.2f\n",
           elapsed, counters->frames,
           counters->frames / (elapsed > 0 ? elapsed : 1),
           counters->bytes, counters->saved, counters->save_errors,
           counters->bytes / (elapsed > 0 ? elapsed : 1) / 1024.0 / 1024.0);

    return 0;
}

int main(int argc, char **argv) {
    options_t options = {
        .device_index = 0,
        .format = FORMAT_MJPEG,
        .width = 640,
        .height = 480,
        .fps = 30,
        .auto_min_data = 0,
        .list_modes = 0,
        .interval_sec = 1,
        .duration_sec = 0,
        .max_frames = 0,
        .save = { .every = 1, .max_saved = 0, .last_only = 0 }
    };

    frame_counters_t counters = {
        .frames = 0,
        .bytes = 0,
        .saved = 0,
        .save_errors = 0,
        .save = NULL,
        .last_frame = { .data = NULL, .data_len = 0, .data_cap = 0 }
    };
    pthread_mutex_init(&counters.last_frame_mutex, NULL);

    // Parse arguments
    for (int i = 1; i < argc; i++) {
        const char *arg = argv[i];

        if (strcmp(arg, "-h") == 0 || strcmp(arg, "--help") == 0) {
            print_help(argv[0]);
            return 0;
        }

        const char *value = NULL;
        if (strncmp(arg, "--", 2) == 0) {
            const char *eq = strchr(arg, '=');
            if (eq != NULL) {
                size_t name_len = eq - arg;
                char name[64];
                if (name_len < sizeof(name) - 1) {
                    memcpy(name, arg, name_len);
                    name[name_len] = '\0';
                    value = eq + 1;
                    arg = name;
                }
            } else if (i + 1 < argc && argv[i + 1][0] != '-') {
                value = argv[++i];
            }
        }

        if (strcmp(arg, "--device") == 0 && value != NULL) {
            options.device_index = atoi(value);
        } else if (strcmp(arg, "--format") == 0 && value != NULL) {
            options.format = parse_format(value);
        } else if (strcmp(arg, "--width") == 0 && value != NULL) {
            options.width = atoi(value);
        } else if (strcmp(arg, "--height") == 0 && value != NULL) {
            options.height = atoi(value);
        } else if (strcmp(arg, "--fps") == 0 && value != NULL) {
            options.fps = atoi(value);
        } else if (strcmp(arg, "--auto-min-data") == 0) {
            options.auto_min_data = 1;
        } else if (strcmp(arg, "--list-modes") == 0) {
            options.list_modes = 1;
        } else if (strcmp(arg, "--interval-sec") == 0 && value != NULL) {
            options.interval_sec = atoi(value);
        } else if (strcmp(arg, "--duration-sec") == 0 && value != NULL) {
            options.duration_sec = atoi(value);
        } else if (strcmp(arg, "--max-frames") == 0 && value != NULL) {
            options.max_frames = atol(value);
        } else if (strcmp(arg, "--save-dir") == 0 && value != NULL) {
            strncpy(options.save.dir, value, sizeof(options.save.dir) - 1);
        } else if (strcmp(arg, "--save-every") == 0 && value != NULL) {
            options.save.every = atol(value);
        } else if (strcmp(arg, "--max-saved") == 0 && value != NULL) {
            options.save.max_saved = atol(value);
        } else if (strcmp(arg, "--save-last") == 0) {
            options.save.last_only = 1;
        } else if (strncmp(arg, "--", 2) == 0) {
            fprintf(stderr, "uvc-fps: error: unknown argument `%s`\n", arg);
            return 1;
        }
    }

    // Validate options
    if (options.save.every == 0) options.save.every = 1;
    if (options.save.max_saved > 0 && options.save.dir[0] == '\0') {
        fprintf(stderr, "uvc-fps: error: --max-saved requires --save-dir\n");
        return 1;
    }
    if (options.save.last_only && options.save.dir[0] == '\0') {
        fprintf(stderr, "uvc-fps: error: --save-last requires --save-dir\n");
        return 1;
    }

    // Print opening message
    if (options.auto_min_data) {
        printf("uvc-fps: opening device=%d format=%s auto_min_data=1 interval_sec=%d\n",
               options.device_index, format_str(options.format), options.interval_sec);
    } else {
        printf("uvc-fps: opening device=%d format=%s size=%dx%d fps=%d interval_sec=%d\n",
               options.device_index, format_str(options.format),
               options.width, options.height, options.fps, options.interval_sec);
    }

    // Create save dir if needed
    if (options.save.dir[0] != '\0') {
        mkdir(options.save.dir, 0755);
        printf("uvc-fps: saving frames dir=%s every=%lu max_saved=%lu\n",
               options.save.dir, options.save.every > 0 ? options.save.every : 1,
               options.save.max_saved > 0 ? options.save.max_saved : MAX_SAVED_DEFAULT);
        if (options.save.last_only) {
            printf("uvc-fps: save mode=last-frame\n");
        }
        counters.save = &options.save;
    }

    // Initialize libuvc
    uvc_context_t *ctx = NULL;
    int result = uvc_init(&ctx, NULL);
    if (result < 0) {
        fprintf(stderr, "uvc-fps: error: uvc_init failed: %s\n", uvc_strerror(result));
        return 1;
    }

    // Find and open device
    uvc_device_t *dev = NULL;
    printf("uvc-fps: entering uvc_get_device_list\n");
    struct timespec list_start;
    get_monotonic_time(&list_start);

    uvc_device_t **dev_list;
    result = uvc_get_device_list(ctx, &dev_list);
    printf("uvc-fps: uvc_get_device_list returned elapsed_ms=%d result=%d\n", ms_elapsed(&list_start), result);

    if (result < 0) {
        fprintf(stderr, "uvc-fps: error: uvc_get_device_list failed: %s\n", uvc_strerror(result));
        uvc_exit(ctx);
        return 1;
    }

    // Count and list all devices
    int dev_count = 0;
    for (int i = 0; dev_list[i] != NULL; i++) {
        dev_count++;
        uint8_t bus = uvc_get_bus_number(dev_list[i]);
        uint8_t address = uvc_get_device_address(dev_list[i]);
        printf("uvc-fps: found device[%d] bus=%d address=%d\n", i, bus, address);
    }
    printf("uvc-fps: total UVC devices found: %d\n", dev_count);

    int found = 0;
    for (int i = 0; dev_list[i] != NULL; i++) {
        if (i == options.device_index) {
            dev = dev_list[i];
            uvc_ref_device(dev);
            found = 1;
            break;
        }
    }

    uvc_free_device_list(dev_list, 1);

    if (!found) {
        fprintf(stderr, "uvc-fps: error: UVC device index %d not found\n", options.device_index);
        uvc_exit(ctx);
        return 1;
    }

    uint8_t bus = uvc_get_bus_number(dev);
    uint8_t address = uvc_get_device_address(dev);

    uvc_device_handle_t *devh = NULL;
    printf("uvc-fps: entering uvc_open device_index=%d bus=%d address=%d\n",
           options.device_index, bus, address);
    struct timespec open_start;
    get_monotonic_time(&open_start);

    result = uvc_open(dev, &devh);
    uvc_unref_device(dev);

    if (result < 0) {
        fprintf(stderr, "uvc-fps: error: uvc_open failed: %s\n", uvc_strerror(result));
        uvc_exit(ctx);
        return 1;
    }

    printf("uvc-fps: uvc_open returned elapsed_ms=%d bus=%d address=%d handle=%p\n",
           ms_elapsed(&open_start), bus, address, (void*)devh);

    // Enumerate modes
    video_mode_t *modes = NULL;
    size_t mode_count = 0;
    if (enumerate_modes(devh, &modes, &mode_count) < 0) {
        fprintf(stderr, "uvc-fps: error: camera did not report any UVC stream modes\n");
        uvc_close(devh);
        uvc_exit(ctx);
        return 1;
    }

    if (options.list_modes || options.auto_min_data) {
        print_modes(modes, mode_count);
    }

    if (options.list_modes) {
        free(modes);
        uvc_close(devh);
        uvc_exit(ctx);
        return 0;
    }

    // Select mode
    uvc_stream_ctrl_t ctrl;
    video_mode_t selected_mode = {0};
    int has_selected = 0;

    if (options.auto_min_data) {
        if (select_min_data_mode(devh, &options, modes, mode_count, &ctrl, &selected_mode) < 0) {
            free(modes);
            uvc_close(devh);
            uvc_exit(ctx);
            return 1;
        }
        has_selected = 1;
    } else {
        if (probe_exact_mode(devh, options.format, options.width, options.height, options.fps, &ctrl) < 0) {
            free(modes);
            uvc_close(devh);
            uvc_exit(ctx);
            return 1;
        }
    }

    if (has_selected) {
        printf("uvc-fps: selected mode format=%s size=%dx%d fps=%d interval=%u "
               "estimated_bytes_per_sec=%lu bitrate=%u max_frame=%u format_index=%u frame_index=%u\n",
               format_str(selected_mode.format), selected_mode.width, selected_mode.height,
               selected_mode.fps, selected_mode.interval, selected_mode.estimated_bytes_per_sec,
               selected_mode.max_bit_rate, selected_mode.max_frame_size,
               selected_mode.format_index, selected_mode.frame_index);
    }

    free(modes);

    // Start streaming
    printf("uvc-fps: entering uvc_start_streaming format_index=%u frame_index=%u interval=%u "
           "max_payload=%u iface=%u\n",
           ctrl.bFormatIndex, ctrl.bFrameIndex, ctrl.dwFrameInterval,
           ctrl.dwMaxPayloadTransferSize, ctrl.bInterfaceNumber);

    struct timespec stream_start;
    get_monotonic_time(&stream_start);

    result = uvc_start_streaming(devh, &ctrl, frame_callback, &counters, 0);

    if (result < 0) {
        fprintf(stderr, "uvc-fps: error: uvc_start_streaming failed: %s\n", uvc_strerror(result));
        uvc_close(devh);
        uvc_exit(ctx);
        return 1;
    }

    printf("uvc-fps: streaming started elapsed_ms=%d\n", ms_elapsed(&stream_start));

    // Report loop
    report_loop(&options, &counters);

    // Stop streaming
    uvc_stop_streaming(devh);

    // Save last frame if needed
    if (counters.save != NULL && counters.save->last_only) {
        save_last_frame(&counters, counters.save);
    }

    // Cleanup
    free(counters.last_frame.data);
    pthread_mutex_destroy(&counters.last_frame_mutex);
    uvc_close(devh);
    uvc_exit(ctx);

    return 0;
}

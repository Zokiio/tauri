#import "include/cef_application_mac.h"
#include "include/capi/cef_app_capi.h"
#include "include/capi/cef_client_capi.h"
#include "include/cef_version.h"

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>

@interface ProbeApp : NSApplication<CefAppProtocol>
@property(nonatomic, getter=isHandlingSendEvent) BOOL handlingSendEvent;
@end

@implementation ProbeApp
- (void)sendEvent:(NSEvent*)event {
  CefScopedSendingEvent sending;
  [super sendEvent:event];
}
@end

struct String {
  cef_string_t value{};
  explicit String(const char* text) {
    cef_string_utf8_to_utf16(text, std::strlen(text), &value);
  }
  ~String() { cef_string_utf16_clear(&value); }
  const cef_string_t* get() const { return &value; }
};

std::string utf8(const cef_string_t* str) {
  if (!str || !str->length) return {};
  cef_string_utf8_t result{};
  cef_string_utf16_to_utf8(str->str, str->length, &result);
  std::string text(result.str, result.length);
  cef_string_utf8_clear(&result);
  return text;
}

template <typename T>
T static_handler() {
  T handler{};
  handler.base.size = sizeof(T);
  handler.base.add_ref = [](cef_base_ref_counted_t*) {};
  handler.base.release = [](cef_base_ref_counted_t*) { return 0; };
  handler.base.has_one_ref = [](cef_base_ref_counted_t*) { return 0; };
  handler.base.has_at_least_one_ref = [](cef_base_ref_counted_t*) { return 1; };
  return handler;
}

static unsigned short proxy_port;
static cef_browser_t* browser;
static cef_request_context_t* context;
static bool complete;
static cef_render_handler_t render = static_handler<cef_render_handler_t>();
static cef_display_handler_t display = static_handler<cef_display_handler_t>();
static cef_request_handler_t request = static_handler<cef_request_handler_t>();
static cef_task_t close_task = static_handler<cef_task_t>();
static cef_client_t client = static_handler<cef_client_t>();
static cef_load_handler_t load = static_handler<cef_load_handler_t>();
static cef_life_span_handler_t life = static_handler<cef_life_span_handler_t>();
static cef_request_context_handler_t context_handler = static_handler<cef_request_context_handler_t>();
static cef_completion_callback_t proxy_ready = static_handler<cef_completion_callback_t>();
static cef_browser_process_handler_t process = static_handler<cef_browser_process_handler_t>();
static cef_app_t app = static_handler<cef_app_t>();

void build_handlers() {
  render.get_view_rect = [](auto*, auto*, auto* rect) { *rect = {0, 0, 640, 480}; };
  render.on_paint = [](auto*, auto*, auto, auto, auto*, auto*, auto, auto) {};

  request.get_auth_credentials = [](auto*, auto* source, auto* origin, int is_proxy,
                                    auto* host, int port, auto*, auto*, auto* callback) {
    std::fprintf(stderr, "AUTH browser=%d proxy=%d host=%s port=%d origin=%s\n",
                 source != nullptr, is_proxy, utf8(host).c_str(), port, utf8(origin).c_str());
    if (source) source->base.release(&source->base);
    const bool match = is_proxy && utf8(host) == "127.0.0.1" && port == proxy_port;
    if (match) callback->cont(callback, String("tb6").get(), String("fixture").get());
    callback->base.release(&callback->base);
    return match ? 1 : 0;
  };

  close_task.execute = [](auto*) {
    auto* host = browser->get_host(browser);
    host->close_browser(host, 1);
    host->base.release(&host->base);
  };

  display.on_console_message = [](auto*, auto* source, auto, auto* message, auto*, auto) {
    source->base.release(&source->base);
    const std::string text = utf8(message);
    if (!text.starts_with("PROBE ")) {
      std::fprintf(stderr, "page console: %s\n", text.c_str());
      return 0;
    }
    std::fprintf(stderr, "%s\n", text.c_str());
    if (text == "PROBE done" || text.starts_with("PROBE fatal")) {
      complete = text == "PROBE done";
      if (!cef_post_task(TID_UI, &close_task)) std::abort();
    }
    return 1;
  };

  load.on_load_end = [](auto*, auto*, auto*, int status) {
    std::fprintf(stderr, "LOAD end status=%d\n", status);
  };
  load.on_load_error = [](auto*, auto*, auto*, auto error, auto* description, auto* url) {
    std::fprintf(stderr, "LOAD error=%d %s %s\n", error, utf8(description).c_str(), utf8(url).c_str());
  };
  client.get_load_handler = [](auto*) { return &load; };
  life.on_before_close = [](auto*, auto* source) {
    source->base.release(&source->base);
    std::fprintf(stderr, "BROWSER closed\n");
    cef_quit_message_loop();
  };
  client.get_request_handler = [](auto*) { return &request; };
  client.get_display_handler = [](auto*) { return &display; };
  client.get_render_handler = [](auto*) { return &render; };
  client.get_life_span_handler = [](auto*) { return &life; };

  proxy_ready.on_complete = [](auto*) {
    std::fprintf(stderr, "proxy ready\n");
    cef_window_info_t window{};
    window.size = sizeof(window);
    window.windowless_rendering_enabled = 1;
    window.runtime_style = CEF_RUNTIME_STYLE_ALLOY;
    cef_browser_settings_t settings{};
    settings.size = sizeof(settings);
    context->base.base.add_ref(&context->base.base);
    browser = cef_browser_host_create_browser_sync(
      &window, &client, String("about:blank").get(), &settings, nullptr, context);
    std::fprintf(stderr, "browser creation finished, success=%d\n", browser != nullptr);
    if (!browser) {
      std::fprintf(stderr, "Can't create CEF browser\n");
      cef_quit_message_loop();
    } else {
      auto* frame = browser->get_main_frame(browser);
      frame->load_url(frame, String(std::getenv("TB6_PAGE")).get());
      frame->base.release(&frame->base);
    }
  };

  context_handler.on_request_context_initialized = [](auto*, auto* rc) {
    context = rc;
    auto* prefs = &context->base;
    auto* dict = cef_dictionary_value_create();
    const auto server = "http://127.0.0.1:" + std::to_string(proxy_port);
    dict->set_string(dict, String("mode").get(), String("fixed_servers").get());
    dict->set_string(dict, String("server").get(), String(server.c_str()).get());
    dict->set_string(dict, String("bypass_list").get(), String("*,<-loopback>,127.0.0.1").get());
    auto* value = cef_value_create();
    value->set_dictionary(value, dict);
    cef_string_t error{};
    const bool applied = prefs->set_preference(prefs, String("proxy").get(), value, &error);
    std::fprintf(stderr, "PREF applied=%d error=%s\n", applied, utf8(&error).c_str());
    cef_string_utf16_clear(&error);
    if (applied) {
      context->close_all_connections(context, &proxy_ready);
    } else {
      cef_quit_message_loop();
    }
  };

  context_handler.get_resource_request_handler = [](auto*, auto* source, auto*, auto* req,
                                                     int, int, auto*, int* disable_default)
      -> cef_resource_request_handler_t* {
    auto* req_url = req->get_url(req);
    const std::string url = utf8(req_url);
    cef_string_userfree_utf16_free(req_url);
    std::fprintf(stderr, "RESOURCE browser=%d default=%d url=%s\n", source != nullptr, !*disable_default, url.c_str());
    *disable_default = 0;
    if (source) source->base.release(&source->base);
    req->base.release(&req->base);
    return nullptr;
  };

  process.on_context_initialized = [](auto*) {
    cef_request_context_settings_t settings{};
    settings.size = sizeof(settings);
    auto* created = cef_request_context_create_context(&settings, &context_handler);
    if (created) {
      created->base.base.release(&created->base.base);
    } else {
      std::fprintf(stderr, "Can't create CEF request context\n");
      cef_quit_message_loop();
    }
  };
  app.get_browser_process_handler = [](auto*) { return &process; };
}

int main(int argc, char** argv) {
  @autoreleasepool {
    if (!cef_api_hash(CEF_API_VERSION_LAST, 0)) return 1;
    const cef_main_args_t args{argc, argv};
    for (int i = 1; i < argc; i++) {
      if (std::string_view(argv[i]).starts_with("--type=")) {
        std::fprintf(stderr, "HELPER %s\n", argv[i]);
        return cef_execute_process(&args, nullptr, nullptr);
      }
    }
    if (!std::getenv("TB6_CACHE") || !std::getenv("TB6_PROXY_PORT") ||
        !std::getenv("TB6_PAGE") || !std::getenv("TB6_HELPER")) return 1;
    std::fprintf(stderr, "CEF %s\n", CEF_VERSION);
    proxy_port = std::atoi(std::getenv("TB6_PROXY_PORT"));
    [ProbeApp sharedApplication];
    build_handlers();
    cef_settings_t settings{};
    settings.size = sizeof(settings);
    settings.no_sandbox = 1;
    settings.windowless_rendering_enabled = 1;
    const char* cache = std::getenv("TB6_CACHE");
    cef_string_utf8_to_utf16(cache, std::strlen(cache), &settings.root_cache_path);
    const char* helper = std::getenv("TB6_HELPER");
    cef_string_utf8_to_utf16(helper, std::strlen(helper), &settings.browser_subprocess_path);
    if (!cef_initialize(&args, &settings, &app, nullptr)) return 1;
    cef_string_utf16_clear(&settings.root_cache_path);
    cef_string_utf16_clear(&settings.browser_subprocess_path);
    cef_run_message_loop();
    if (browser) browser->base.release(&browser->base);
    if (context) context->base.base.release(&context->base.base);
    cef_shutdown();
    return complete ? 0 : 1;
  }
}

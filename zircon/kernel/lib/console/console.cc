// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008-2009 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "lib/console.h"

#if CONSOLE_ENABLED

#include <assert.h>
#include <ctype.h>
#include <debug.h>
#include <lib/boot-options/boot-options.h>
#include <lib/debuglog.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <trace.h>
#include <zircon/compiler.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <kernel/mutex.h>
#include <kernel/thread.h>
#include <ktl/algorithm.h>
#include <ktl/unique_ptr.h>
#include <lk/init.h>

#include <ktl/enforce.h>

#define LINE_LEN 128

#define PANIC_LINE_LEN 32

#define MAX_NUM_ARGS 16

#define LOCAL_TRACE 0

#define WHITESPACE " \t"

/* debug buffer */
static char* debug_buffer;

/* echo commands? */
extern "C" bool rust_console_get_echo();
extern "C" bool rust_console_get_exit();
extern "C" void rust_console_set_exit(bool val);
extern "C" const cmd* rust_console_match_command(const char* name, uint8_t availability_mask);
extern "C" int cpp_console_get_lastresult();

/* command processor state */
namespace {
DECLARE_SINGLETON_MUTEX(CommandLock);
}  // namespace
static int lastresult;

// FFI bindings for console history. The Rust implementation is the source of truth and safely
// handles cases where history is disabled or not compiled in.
extern "C" bool rust_console_is_history_enabled(void);
extern "C" void rust_console_init_history(void);
extern "C" void rust_console_add_history(const char* line);
extern "C" uint32_t rust_console_start_history_cursor(void);
extern "C" const char* rust_console_next_history(uint32_t* cursor);
extern "C" const char* rust_console_prev_history(uint32_t* cursor);

static void console_init(uint level) { rust_console_init_history(); }

LK_INIT_HOOK(console, console_init, LK_INIT_LEVEL_HEAP)

static inline int cgetchar(void) {
  char c;
  int r = platform_dgetc(&c, true);
  return (r < 0) ? r : c;
}
static inline void cputchar(char c) { platform_dputc(c); }
static inline void cputs(const char* s) { platform_dputs_thread(s, strlen(s)); }

static int read_debug_line(const char** outbuffer, void* cookie) {
  size_t pos = 0;
  int escape_level = 0;
  uint32_t history_cursor = rust_console_start_history_cursor();

  char* buffer = debug_buffer;

  for (;;) {
    /* loop until we get a char */
    int ci;
    if ((ci = cgetchar()) <= 0)
      continue;

    char c = static_cast<char>(ci);

    //      TRACEF("c = 0x%hhx\n", c);

    if (escape_level == 0) {
      switch (c) {
        case '\r':
        case '\n':
          if (rust_console_get_echo())
            cputchar('\n');
          goto done;

        case 0x7f:  // backspace or delete
        case 0x8:
          if (pos > 0) {
            pos--;
            cputs("\b \b");  // wipe out a character
          }
          break;

        case 0x1b:  // escape
          escape_level++;
          break;

        default:
          buffer[pos++] = c;
          if (rust_console_get_echo())
            cputchar(c);
      }
    } else if (escape_level == 1) {
      // inside an escape, look for '['
      if (c == '[') {
        escape_level++;
      } else {
        // we didn't get it, abort
        escape_level = 0;
      }
    } else {  // escape_level > 1
      switch (c) {
        case 67:  // right arrow
          buffer[pos++] = ' ';
          if (rust_console_get_echo())
            cputchar(' ');
          break;
        case 68:  // left arrow
          if (pos > 0) {
            pos--;
            if (rust_console_get_echo()) {
              cputs("\b \b");  // wipe out a character
            }
          }
          break;
        case 65:  // up arrow -- previous history
        case 66:  // down arrow -- next history
          if (!rust_console_is_history_enabled()) {
            break;
          }
          // wipe out the current line
          while (pos > 0) {
            pos--;
            if (rust_console_get_echo()) {
              cputs("\b \b");  // wipe out a character
            }
          }

          if (c == 65)
            strlcpy(buffer, rust_console_prev_history(&history_cursor), LINE_LEN);
          else
            strlcpy(buffer, rust_console_next_history(&history_cursor), LINE_LEN);
          pos = strlen(buffer);
          if (rust_console_get_echo())
            cputs(buffer);
          break;
        default:
          break;
      }
      escape_level = 0;
    }

    /* end of line. */
    if (pos == (LINE_LEN - 1)) {
      cputs("\nerror: line too long\n");
      pos = 0;
      goto done;
    }
  }

done:
  //  dprintf("returning pos %d\n", pos);

  // null terminate
  buffer[pos] = 0;

  // add to history
  rust_console_add_history(buffer);

  // return a pointer to our buffer
  *outbuffer = buffer;

  return static_cast<int>(pos);
}

static int tokenize_command(const char* inbuffer, const char** continuebuffer, char* buffer,
                            size_t buflen, cmd_args* args, int arg_count) {
  size_t inpos;
  size_t outpos;
  int arg;
  enum {
    INITIAL = 0,
    NEXT_FIELD,
    SPACE,
    IN_SPACE,
    TOKEN,
    IN_TOKEN,
    QUOTED_TOKEN,
    IN_QUOTED_TOKEN,
    VAR,
    IN_VAR,
    COMMAND_SEP,
  } state;
  char varname[128];
  size_t varnamepos;

  inpos = 0;
  outpos = 0;
  arg = 0;
  varnamepos = 0;
  state = INITIAL;
  *continuebuffer = NULL;

  for (;;) {
    char c = inbuffer[inpos];

    //      dprintf(SPEW, "c 0x%hhx state %d arg %d inpos %zu pos %zu\n", c, state, arg, inpos,
    //      outpos);

    switch (state) {
      case INITIAL:
      case NEXT_FIELD:
        if (c == '\0')
          goto done;
        if (isspace(c))
          state = SPACE;
        else if (c == ';')
          state = COMMAND_SEP;
        else
          state = TOKEN;
        break;
      case SPACE:
        state = IN_SPACE;
        break;
      case IN_SPACE:
        if (c == '\0')
          goto done;
        if (c == ';') {
          state = COMMAND_SEP;
        } else if (!isspace(c)) {
          state = TOKEN;
        } else {
          inpos++;  // consume the space
        }
        break;
      case TOKEN:
        // start of a token
        DEBUG_ASSERT(c != '\0');
        if (c == '"') {
          // start of a quoted token
          state = QUOTED_TOKEN;
        } else if (c == '$') {
          // start of a variable
          state = VAR;
        } else {
          // regular, unquoted token
          state = IN_TOKEN;
          args[arg].str = &buffer[outpos];
        }
        break;
      case IN_TOKEN:
        if (c == '\0') {
          arg++;
          goto done;
        }
        if (isspace(c) || c == ';') {
          arg++;
          buffer[outpos] = 0;
          outpos++;
          /* are we out of tokens? */
          if (arg == arg_count)
            goto done;
          state = NEXT_FIELD;
        } else {
          buffer[outpos] = c;
          outpos++;
          inpos++;
        }
        break;
      case QUOTED_TOKEN:
        // start of a quoted token
        DEBUG_ASSERT(c == '"');

        state = IN_QUOTED_TOKEN;
        args[arg].str = &buffer[outpos];
        inpos++;  // consume the quote
        break;
      case IN_QUOTED_TOKEN:
        if (c == '\0') {
          arg++;
          goto done;
        }
        if (c == '"') {
          arg++;
          buffer[outpos] = 0;
          outpos++;
          /* are we out of tokens? */
          if (arg == arg_count)
            goto done;

          state = NEXT_FIELD;
        }
        buffer[outpos] = c;
        outpos++;
        inpos++;
        break;
      case VAR:
        DEBUG_ASSERT(c == '$');

        state = IN_VAR;
        args[arg].str = &buffer[outpos];
        inpos++;  // consume the dollar sign

        // initialize the place to store the variable name
        varnamepos = 0;
        break;
      case IN_VAR:
        if (c == '\0' || isspace(c) || c == ';') {
          // hit the end of variable, look it up and stick it inline
          varname[varnamepos] = 0;
#if WITH_LIB_ENV
          int rc = env_get(varname, &buffer[outpos], buflen - outpos);
#else
          (void)varname[0];  // nuke a warning
          int rc = -1;
#endif
          if (rc < 0) {
            buffer[outpos++] = '0';
            buffer[outpos++] = 0;
          } else {
            outpos += strlen(&buffer[outpos]) + 1;
          }
          arg++;
          /* are we out of tokens? */
          if (arg == arg_count)
            goto done;

          state = NEXT_FIELD;
        } else {
          varname[varnamepos] = c;
          varnamepos++;
          inpos++;
        }
        break;
      case COMMAND_SEP:
        // we hit a ;, so terminate the command and pass the remainder of the command back in
        // continuebuffer
        DEBUG_ASSERT(c == ';');

        inpos++;  // consume the ';'
        *continuebuffer = &inbuffer[inpos];
        goto done;
    }
  }

done:
  buffer[outpos] = 0;
  return arg;
}

static void convert_args(int argc, cmd_args* argv) {
  int i;

  for (i = 0; i < argc; i++) {
    unsigned long u = strtoul(argv[i].str, nullptr, 0);
    argv[i].u = u;
    argv[i].p = (void*)u;
    argv[i].i = strtol(argv[i].str, nullptr, 0);

    if (!strcmp(argv[i].str, "true") || !strcmp(argv[i].str, "on")) {
      argv[i].b = true;
    } else if (!strcmp(argv[i].str, "false") || !strcmp(argv[i].str, "off")) {
      argv[i].b = false;
    } else {
      argv[i].b = (argv[i].u == 0) ? false : true;
    }
  }
}

static zx_status_t command_loop(int (*get_line)(const char**, void*), void* get_line_cookie,
                                bool showprompt, bool locked) TA_NO_THREAD_SAFETY_ANALYSIS {
  zx_status_t ret = ZX_OK;
  bool exit;
#if WITH_LIB_ENV
  bool report_result;
#endif
  cmd_args* args = NULL;
  const char* buffer;
  const char* continuebuffer;
  char* outbuf = NULL;

  args = (cmd_args*)malloc(MAX_NUM_ARGS * sizeof(cmd_args));
  if (unlikely(args == NULL)) {
    return ZX_ERR_NO_MEMORY;
  }

  const size_t outbuflen = 1024;
  outbuf = static_cast<char*>(malloc(outbuflen));
  if (unlikely(outbuf == NULL)) {
    free(args);
    return ZX_ERR_NO_MEMORY;
  }

  exit = false;
  continuebuffer = NULL;
  while (!exit) {
    // read a new line if it hadn't been split previously and passed back from tokenize_command
    if (continuebuffer == NULL) {
      if (showprompt)
        cputs("] ");

      int len = get_line(&buffer, get_line_cookie);
      if (len < 0)
        break;
      if (len == 0)
        continue;
    } else {
      buffer = continuebuffer;
    }

    //      dprintf("line = '%s'\n", buffer);

    /* tokenize the line */
    int argc = tokenize_command(buffer, &continuebuffer, outbuf, outbuflen, args, MAX_NUM_ARGS);
    if (argc < 0) {
      if (showprompt)
        printf("syntax error\n");
      continue;
    } else if (argc == 0) {
      continue;
    }

    //      dprintf("after tokenize: argc %d\n", argc);
    //      for (int i = 0; i < argc; i++)
    //          dprintf("%d: '%s'\n", i, args[i].str);

    /* convert the args */
    convert_args(argc, args);

    /* try to match the command */
    const cmd* command = rust_console_match_command(args[0].str, CMD_AVAIL_NORMAL);
    if (!command) {
      printf("command \"%s\" not found\n", args[0].str);
      lastresult = -1;
      continue;
    }

    if (!locked)
      CommandLock::Get()->lock().Acquire();

    rust_console_set_exit(false);
    lastresult = command->cmd_callback(argc, args, 0);

#if WITH_LIB_ENV
    bool report_result;
    env_get_bool("reportresult", &report_result, false);
    if (report_result) {
      if (lastresult < 0)
        printf("FAIL %d\n", lastresult);
      else
        printf("PASS %d\n", lastresult);
    }
#endif

#if WITH_LIB_ENV
    // stuff the result in an environment var
    env_set_int("?", lastresult, true);
#endif

    // someone must have called console_set_exit(true) inside the command
    if (rust_console_get_exit()) {
      exit = true;
      rust_console_set_exit(false);
      ret = ZX_ERR_CANCELED;
    }

    if (!locked)
      CommandLock::Get()->lock().Release();
  }

  free(outbuf);
  free(args);
  return ret;
}

static void console_start(void) {
  debug_buffer = static_cast<char*>(malloc(LINE_LEN));

  dprintf(INFO, "entering main console loop\n");

  while (command_loop(&read_debug_line, NULL, true, false) == ZX_OK)
    ;

  dprintf(INFO, "exiting main console loop\n");

  free(debug_buffer);
}

struct line_read_struct {
  const char* string;
  int pos;
  char* buffer;
  size_t buflen;
};

static int fetch_next_line(const char** buffer, void* cookie) {
  struct line_read_struct* lineread = (struct line_read_struct*)cookie;

  // we're done
  if (lineread->string[lineread->pos] == 0)
    return -1;

  size_t bufpos = 0;
  while (lineread->string[lineread->pos] != 0) {
    if (char c = lineread->string[lineread->pos]; c == '\n' || c == ';') {
      lineread->pos++;
      break;
    }
    if (bufpos == (lineread->buflen - 1))
      break;
    lineread->buffer[bufpos] = lineread->string[lineread->pos];
    lineread->pos++;
    bufpos++;
  }
  lineread->buffer[bufpos] = 0;

  // add to history
  rust_console_add_history(lineread->buffer);

  *buffer = lineread->buffer;

  return static_cast<int>(bufpos);
}

static int console_run_script_etc(const char* string, bool locked) {
  struct line_read_struct lineread;

  lineread.string = string;
  lineread.pos = 0;
  lineread.buffer = static_cast<char*>(malloc(LINE_LEN));
  lineread.buflen = LINE_LEN;

  command_loop(&fetch_next_line, (void*)&lineread, false, locked);

  free(lineread.buffer);

  return lastresult;
}

int console_run_script(const char* string) { return console_run_script_etc(string, false); }

int console_run_script_locked(const char* string) { return console_run_script_etc(string, true); }

static void panic_putc(char c) { platform_pputc(c); }

static void panic_puts(const char* str) {
  for (;;) {
    char c = *str++;
    if (c == 0) {
      break;
    }
    platform_pputc(c);
  }
}

static int panic_getc(void) {
  char c;
  if (platform_pgetc(&c) < 0) {
    return -1;
  } else {
    return c;
  }
}

static void read_line_panic(char* buffer, const size_t len) {
  size_t pos = 0;

  for (;;) {
    int ci;
    if ((ci = panic_getc()) < 0) {
      continue;
    }

    char c = static_cast<char>(ci);

    switch (c) {
      case '\r':
      case '\n':
        panic_putc('\n');
        goto done;
      case 0x7f:  // backspace or delete
      case 0x8:
        if (pos > 0) {
          pos--;
          panic_puts("\b \b");  // wipe out a character
        }
        break;
      default:
        buffer[pos++] = c;
        panic_putc(c);
    }
    if (pos == (len - 1)) {
      panic_puts("\nerror: line too long\n");
      pos = 0;
      goto done;
    }
  }
done:
  buffer[pos] = 0;
}

void panic_shell_start(void) {
  dprintf(INFO, "entering panic shell loop\n");
  char input_buffer[PANIC_LINE_LEN];
  cmd_args args[MAX_NUM_ARGS];

  // Panic may have been triggered via an interrupt/exception path, where blocking would normally
  // disallowed. As some panic shell commands need to take mutexes and perform other operations that
  // would otherwise be invalid if blocking is disallowed we re-allow it.
  arch_set_blocking_disallowed(false);

  for (;;) {
    panic_puts("! ");
    read_line_panic(input_buffer, PANIC_LINE_LEN);

    int argc;
    char* tok = strtok(input_buffer, WHITESPACE);
    for (argc = 0; argc < MAX_NUM_ARGS; argc++) {
      if (tok == NULL) {
        break;
      }
      args[argc].str = tok;
      tok = strtok(NULL, WHITESPACE);
    }

    if (argc == 0) {
      continue;
    }

    convert_args(argc, args);

    const cmd* command = rust_console_match_command(args[0].str, CMD_AVAIL_PANIC);
    if (!command) {
      panic_puts("command not found\n");
      continue;
    }

    command->cmd_callback(argc, args, CMD_FLAG_PANIC);
  }
}

static constexpr TimerSlack kSlack{ZX_MSEC(10), TIMER_SLACK_CENTER};

void RecurringCallback::CallbackWrapper(Timer* t, zx_instant_mono_t now, void* arg) {
  auto cb = static_cast<RecurringCallback*>(arg);
  cb->func_();

  {
    Guard<SpinLock, IrqSave> guard{&cb->lock_};

    if (cb->started_) {
      const Deadline deadline(zx_time_add_duration(now, ZX_SEC(1)), kSlack);
      t->Set(deadline, CallbackWrapper, arg);
    }
  }

  // reschedule to give the debuglog a chance to run
  Thread::Current::preemption_state().PreemptSetPending();
}

void RecurringCallback::Toggle() {
  Guard<SpinLock, IrqSave> guard{&lock_};

  if (!started_) {
    const Deadline deadline = Deadline::after_mono(ZX_SEC(1), kSlack);
    // start the timer
    timer_.Set(deadline, CallbackWrapper, static_cast<void*>(this));
    started_ = true;
  } else {
    timer_.Cancel();
    started_ = false;
  }
}

void kernel_shell_init() {
  if (!BootOptions::Get()->shell_script.empty()) {
    SmallString script = BootOptions::Get()->shell_script;
    for (char* p = strchr(script.data(), '+'); p; p = strchr(p + 1, '+')) {
      *p = ' ';
    }
    console_run_script(script.data());
  }
  if (BootOptions::Get()->shell) {
    console_start();
  }
}

// FFI exports.
extern "C" int cpp_console_get_lastresult() { return lastresult; }

#endif  // #if CONSOLE_ENABLED

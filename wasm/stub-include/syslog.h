/* Minimal syslog.h stub for wasm32-wasip1. libpg_query's vendored elog.c includes
 * <syslog.h> (HAVE_SYSLOG) but the syslog log destination is never enabled by
 * libpg_query entry points; calls are macro-discarded so no link symbols arise. */
#pragma once
#define LOG_EMERG 0
#define LOG_ALERT 1
#define LOG_CRIT 2
#define LOG_ERR 3
#define LOG_WARNING 4
#define LOG_NOTICE 5
#define LOG_INFO 6
#define LOG_DEBUG 7
#define LOG_PID 0x01
#define LOG_NDELAY 0x08
#define LOG_LOCAL0 (16 << 3)
#define LOG_LOCAL1 (17 << 3)
#define LOG_LOCAL2 (18 << 3)
#define LOG_LOCAL3 (19 << 3)
#define LOG_LOCAL4 (20 << 3)
#define LOG_LOCAL5 (21 << 3)
#define LOG_LOCAL6 (22 << 3)
#define LOG_LOCAL7 (23 << 3)
#define LOG_USER (1 << 3)
#define openlog(ident, option, facility) ((void)0)
#define syslog(priority, ...) ((void)0)
#define closelog() ((void)0)

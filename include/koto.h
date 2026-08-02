#ifndef KOTO_H
#define KOTO_H

#ifdef __cplusplus
extern "C" {
#endif

/* Returns the ABI contract version. */
int koto_abi_version(void);

/* Validates a UTF-8, NUL-terminated basm program. Returns 0 or 8. */
int koto_parse_basm(const char *source);

#ifdef __cplusplus
}
#endif

#endif /* KOTO_H */

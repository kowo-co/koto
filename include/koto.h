#ifndef KOTO_H
#define KOTO_H

#ifdef __cplusplus
extern "C" {
#endif

/* Returns the ABI contract version. */
int koto_abi_version(void);

/* Validates a UTF-8, NUL-terminated basm program. Returns 0 or 8. */
int koto_parse_basm(const char *source);

/* Executes basm with a comma-separated capability set. Writes $out when an
 * output buffer is supplied. Returns a koto exit code. */
int koto_run_basm(const char *source, const char *capabilities, char *output, unsigned long output_len);

/* Executes basm; returns a malloc-style UTF-8 JSON envelope the caller must
 * release with koto_free. Never returns NULL. */
char *koto_run_basm_json(const char *source, const char *capabilities);

/* Releases a buffer returned by koto_run_basm_json. */
void koto_free(char *ptr);

#ifdef __cplusplus
}
#endif

#endif /* KOTO_H */

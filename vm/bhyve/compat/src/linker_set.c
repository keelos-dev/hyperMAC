/* The sets of compat/include/sys/linker_set.h: one growing array of pointers per set name. */
#include <err.h>
#include <stdlib.h>
#include <string.h>

#include <sys/linker_set.h>

struct lset {
	const char	*name;
	void		**items;
	size_t		n, cap;
	struct lset	*next;
};

/* Constructors run before main and in one thread: no lock. */
static struct lset *sets;

static struct lset *
lset_find(const char *name, int create)
{
	struct lset *s;

	for (s = sets; s != NULL; s = s->next)
		if (strcmp(s->name, name) == 0)
			return (s);
	if (!create)
		return (NULL);
	s = calloc(1, sizeof(*s));
	if (s == NULL)
		err(1, "linker set %s", name);
	s->name = name;
	s->next = sets;
	sets = s;
	return (s);
}

void
__linker_set_add(const char *set, const void *sym)
{
	struct lset *s;

	s = lset_find(set, 1);
	if (s->n == s->cap) {
		s->cap = s->cap == 0 ? 16 : s->cap * 2;
		s->items = realloc(s->items, s->cap * sizeof(void *));
		if (s->items == NULL)
			err(1, "linker set %s", set);
	}
	s->items[s->n++] = __DECONST(void *, sym);
}

void **
__linker_set_begin(const char *set)
{
	struct lset *s;

	s = lset_find(set, 0);
	return (s == NULL ? NULL : s->items);
}

void **
__linker_set_limit(const char *set)
{
	struct lset *s;

	s = lset_find(set, 0);
	return (s == NULL ? NULL : s->items + s->n);
}

/*
 * Unnamed POSIX semaphores for macOS, where sem_init() only fails (ENOSYS): the calls bhyve uses,
 * on a mutex and a condition variable. Without this pci_nvme.c's request pool never blocks and
 * asserts when it runs dry.
 */
#ifndef _BHYVE_SEMAPHORE_H_
#define	_BHYVE_SEMAPHORE_H_

#include <pthread.h>

typedef struct {
	pthread_mutex_t	mtx;
	pthread_cond_t	cv;
	unsigned int	value;
} bhyve_sem_t;

#define	sem_t		bhyve_sem_t
#define	sem_init	bhyve_sem_init
#define	sem_destroy	bhyve_sem_destroy
#define	sem_wait	bhyve_sem_wait
#define	sem_post	bhyve_sem_post

static inline int
bhyve_sem_init(bhyve_sem_t *s, int pshared __attribute__((unused)), unsigned int value)
{
	pthread_mutex_init(&s->mtx, NULL);
	pthread_cond_init(&s->cv, NULL);
	s->value = value;
	return (0);
}

static inline int
bhyve_sem_destroy(bhyve_sem_t *s)
{
	pthread_cond_destroy(&s->cv);
	pthread_mutex_destroy(&s->mtx);
	return (0);
}

static inline int
bhyve_sem_wait(bhyve_sem_t *s)
{
	pthread_mutex_lock(&s->mtx);
	while (s->value == 0)
		pthread_cond_wait(&s->cv, &s->mtx);
	s->value--;
	pthread_mutex_unlock(&s->mtx);
	return (0);
}

static inline int
bhyve_sem_post(bhyve_sem_t *s)
{
	pthread_mutex_lock(&s->mtx);
	s->value++;
	pthread_cond_signal(&s->cv);
	pthread_mutex_unlock(&s->mtx);
	return (0);
}

#endif /* _BHYVE_SEMAPHORE_H_ */

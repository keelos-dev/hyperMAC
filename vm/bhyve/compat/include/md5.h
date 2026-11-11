/* FreeBSD's libmd MD5 names on CommonCrypto. */
#ifndef _MD5_H_
#define	_MD5_H_

#define	COMMON_DIGEST_FOR_OPENSSL
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
#include <CommonCrypto/CommonDigest.h>
#pragma clang diagnostic pop

#define	MD5Init(c)		CC_MD5_Init(c)
#define	MD5Update(c, d, l)	CC_MD5_Update(c, d, (CC_LONG)(l))
#define	MD5Final(md, c)		CC_MD5_Final(md, c)

#endif /* _MD5_H_ */

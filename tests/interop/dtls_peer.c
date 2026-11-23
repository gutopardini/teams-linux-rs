/* Own synthetic DTLS1.2/SRTP server using the public OpenSSL API. Loopback only;
 * pins its client's SHA256 fingerprint supplied by the test; ephemeral in RAM. */
#include <openssl/ssl.h>
#include <openssl/x509.h>
#include <openssl/ec.h>
#include <openssl/crypto.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <sys/time.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
static unsigned char expected[32];
static int verify(int verified, X509_STORE_CTX *store) {
    (void)verified;
    if(X509_STORE_CTX_get_error_depth(store)>4) return 0;
    if(X509_STORE_CTX_get_error_depth(store)!=0) return 1;
    unsigned char digest[32];unsigned int len=0;
    return X509_digest(X509_STORE_CTX_get_current_cert(store),EVP_sha256(),digest,&len)==1 &&
        len==32 && CRYPTO_memcmp(expected,digest,32)==0;
}
int main(int argc,char **argv) {
    if(argc!=2||strlen(argv[1])!=103||strncmp(argv[1],"sha-256 ",8)!=0) return 2;
    for(int i=0;i<32;i++) {unsigned int b;if(sscanf(argv[1]+8+i*3,"%2x",&b)!=1)return 3;expected[i]=(unsigned char)b;}
    int result=4,sock=-1;SSL_CTX *ctx=NULL;SSL *ssl=NULL;EVP_PKEY *key=NULL;EVP_PKEY_CTX *generation=NULL;X509 *cert=NULL;
    generation=EVP_PKEY_CTX_new_id(EVP_PKEY_EC,NULL);
    if(!generation||EVP_PKEY_keygen_init(generation)<=0||EVP_PKEY_CTX_set_ec_paramgen_curve_nid(generation,NID_X9_62_prime256v1)<=0||EVP_PKEY_keygen(generation,&key)<=0)goto done;
    cert=X509_new();if(!cert)goto done;
    X509_set_version(cert,2);ASN1_INTEGER_set(X509_get_serialNumber(cert),1);
    X509_gmtime_adj(X509_getm_notBefore(cert),-60);X509_gmtime_adj(X509_getm_notAfter(cert),3600);
    X509_NAME *name=X509_get_subject_name(cert);
    if(!X509_NAME_add_entry_by_txt(name,"CN",MBSTRING_ASC,(const unsigned char *)"interop.native.invalid",-1,-1,0)||!X509_set_issuer_name(cert,name)||!X509_set_pubkey(cert,key)||!X509_sign(cert,key,EVP_sha256()))goto done;
    ctx=SSL_CTX_new(DTLS_server_method());if(!ctx)goto done;
    SSL_CTX_set_min_proto_version(ctx,DTLS1_2_VERSION);SSL_CTX_set_max_proto_version(ctx,DTLS1_2_VERSION);
    SSL_CTX_set_options(ctx,SSL_OP_NO_QUERY_MTU|SSL_OP_NO_RENEGOTIATION|SSL_OP_NO_COMPRESSION);
    SSL_CTX_set_max_cert_list(ctx,16384);SSL_CTX_set_verify_depth(ctx,4);
    SSL_CTX_set_verify(ctx,SSL_VERIFY_PEER|SSL_VERIFY_FAIL_IF_NO_PEER_CERT,verify);
    if(!SSL_CTX_use_certificate(ctx,cert)||!SSL_CTX_use_PrivateKey(ctx,key)||SSL_CTX_set_tlsext_use_srtp(ctx,"SRTP_AES128_CM_SHA1_80")!=0)goto done;
    sock=socket(AF_INET,SOCK_DGRAM,0);if(sock<0)goto done;
    struct sockaddr_in local={0};local.sin_family=AF_INET;local.sin_addr.s_addr=htonl(INADDR_LOOPBACK);
    if(bind(sock,(struct sockaddr *)&local,sizeof(local))!=0)goto done;
    socklen_t size=sizeof(local);if(getsockname(sock,(struct sockaddr *)&local,&size)!=0)goto done;
    struct timeval timeout={5,0};if(setsockopt(sock,SOL_SOCKET,SO_RCVTIMEO,&timeout,sizeof(timeout))!=0)goto done;
    unsigned char digest[32];unsigned int len=0;if(!X509_digest(cert,EVP_sha256(),digest,&len)||len!=32)goto done;
    printf("READY %u sha-256 ",ntohs(local.sin_port));for(int i=0;i<32;i++)printf(i?":%02X":"%02X",digest[i]);puts("");fflush(stdout);
    struct sockaddr_in peer={0};size=sizeof(peer);unsigned char peek[1];
    if(recvfrom(sock,peek,sizeof(peek),MSG_PEEK,(struct sockaddr *)&peer,&size)<=0||connect(sock,(struct sockaddr *)&peer,sizeof(peer))!=0)goto done;
    ssl=SSL_new(ctx);if(!ssl)goto done;
    BIO *bio=BIO_new_dgram(sock,BIO_NOCLOSE);if(!bio)goto done;
    BIO_ctrl(bio,BIO_CTRL_DGRAM_SET_CONNECTED,0,&peer);SSL_set_bio(ssl,bio,bio);SSL_set_mtu(ssl,1200);
    if(SSL_accept(ssl)!=1)goto done;
    const SRTP_PROTECTION_PROFILE *profile=SSL_get_selected_srtp_profile(ssl);
    if(!profile||strcmp(profile->name,"SRTP_AES128_CM_SHA1_80")!=0)goto done;
    unsigned char exported[60];if(!SSL_export_keying_material(ssl,exported,sizeof(exported),"EXTRACTOR-dtls_srtp",19,NULL,0,0))goto done;
    printf("EXPORTED ");for(int i=0;i<60;i++)printf("%02x",exported[i]);puts("");fflush(stdout);
    OPENSSL_cleanse(exported,sizeof(exported));result=0;
done:
    SSL_free(ssl);SSL_CTX_free(ctx);X509_free(cert);EVP_PKEY_free(key);EVP_PKEY_CTX_free(generation);if(sock>=0)close(sock);return result;
}

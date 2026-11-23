/* Own synthetic interop peer. Builds against the official libSRTP2 public
 * header; no account configuration, devices or network access. */
#include <srtp.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
static void output(const unsigned char *data,int len) {
    for(int i=0;i<len;i++) printf("%02x",data[i]);
    puts("");
}
int main(int argc,char **argv) {
    if(argc!=2||srtp_init()!=srtp_err_status_ok) return 2;
    unsigned char key[30];memset(key,0x11,16);memset(key+16,0x22,14);
    srtp_policy_t policy;memset(&policy,0,sizeof(policy));
    srtp_crypto_policy_set_rtp_default(&policy.rtp);
    srtp_crypto_policy_set_rtcp_default(&policy.rtcp);
    policy.ssrc.type=strncmp(argv[1],"unprotect",9)==0?ssrc_any_inbound:ssrc_any_outbound;
    policy.key=key;policy.window_size=128;
    srtp_t session=NULL;if(srtp_create(&session,&policy)!=srtp_err_status_ok)return 3;
    int code=0;
    if(strcmp(argv[1],"protect")==0) {
        /* RR + CNAME sdes, SSRC7. Explicit synthetic wire bytes. */
        unsigned char packet[2048]={0x80,201,0,1,0,0,0,7,0x81,202,0,3,0,0,0,7,1,5,'t','e','s','t','0',0};
        int len=24;code=srtp_protect_rtcp(session,packet,&len);if(!code)output(packet,len);
    } else if(strcmp(argv[1],"rtp")==0) {
        for(int i=0;i<3;i++) {
            unsigned char packet[2048]={0x80,0,0,0,0,0,0,0,0,0,0,7};
            packet[2]=i==0?255:0;packet[3]=i==0?255:0;packet[11]=i==2?8:7;
            memset(packet+12,255,160);int len=172;code=srtp_protect(session,packet,&len);
            if(code) { break; }
            output(packet,len);
        }
    } else if(strcmp(argv[1],"unprotect")==0) {
        char hex[4098];unsigned char packet[2048];
        if(!fgets(hex,sizeof(hex),stdin)){code=4;}else {
            size_t n=strcspn(hex,"\r\n");if(n%2||n>sizeof(packet)*2){code=5;}else {
                int len=(int)n/2;
                for(int i=0;i<len;i++) {unsigned int value;if(sscanf(hex+i*2,"%2x",&value)!=1){code=6;break;}packet[i]=(unsigned char)value;}
                if(!code)code=srtp_unprotect_rtcp(session,packet,&len);
                if(!code)output(packet,len);
            }
        }
    } else code=7;
    srtp_dealloc(session);srtp_shutdown();return code;
}

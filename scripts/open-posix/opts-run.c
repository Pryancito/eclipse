/* opts-run LIST BASEDIR TIMEOUT [SKIPFILE]: runs each test of LIST (paths
 * relative to BASEDIR) in its own dir and process group, kills the group after
 * TIMEOUT s, prints "R <code> <secs> <test>" per test. Codes: exit status,
 * 128+sig for a signal, 124 for hung. Output of the test goes to /tmp/opts.out
 * and its first lines are printed for anything but PASS. */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <signal.h>
#include <fcntl.h>
#include <time.h>
#include <errno.h>
#include <sys/wait.h>
#include <libgen.h>

static double now(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec+t.tv_nsec/1e9;}

int main(int argc,char**argv){
  if(argc<4){fprintf(stderr,"usage\n");return 2;}
  FILE*l=fopen(argv[1],"r"); if(!l){perror("list");return 2;}
  const char*base=argv[2]; int to=atoi(argv[3]);
  char line[1024];
  setvbuf(stdout,NULL,_IOLBF,0);
  signal(SIGTTOU,SIG_IGN); signal(SIGTTIN,SIG_IGN);
  while(fgets(line,sizeof line,l)){
    line[strcspn(line,"\n")]=0; if(!*line) continue;
    char path[1200],dir[1200],args[1300];
    snprintf(path,sizeof path,"%s/%s",base,line);
    strcpy(dir,path); char*d=dirname(dir);
    char*argvv[16]; int ac=0; char*name=strrchr(path,'/')+1;
    argvv[ac++]=name;
    snprintf(args,sizeof args,"%s",path); char*dot=strstr(args,".run-test"); if(dot) strcpy(dot,".args");
    char abuf[512]={0}; FILE*af=fopen(args,"r");
    if(af){ if(fgets(abuf,sizeof abuf,af)){ char*s=strtok(abuf," \t\n"); while(s&&ac<15){argvv[ac++]=s;s=strtok(NULL," \t\n");} } fclose(af);}
    argvv[ac]=0;
    double t0=now();
    pid_t p=fork();
    if(p==0){
      setpgid(0,0);
      if(chdir(d)) _exit(126);
      int fd=open("/tmp/opts.out",O_WRONLY|O_CREAT|O_TRUNC,0644);
      if(fd>=0){dup2(fd,1);dup2(fd,2);close(fd);}
      int nul=open("/dev/null",O_RDONLY); if(nul>=0){dup2(nul,0);close(nul);}
      sigset_t s; sigemptyset(&s); sigprocmask(SIG_SETMASK,&s,0);
      for(int i=1;i<32;i++) signal(i,SIG_DFL);
      execv(path,argvv); _exit(127);
    }
    setpgid(p,p);
    int st=0,code=-1; 
    while(1){
      pid_t r=waitpid(p,&st,WNOHANG);
      if(r==p){ code=WIFEXITED(st)?WEXITSTATUS(st):128+WTERMSIG(st); break;}
      if(r<0&&errno!=EINTR){code=125;break;}
      if(now()-t0>to){ kill(-p,SIGKILL); kill(p,SIGKILL); waitpid(p,&st,0); code=124; break;}
      usleep(20000);
    }
    kill(-p,SIGKILL); /* leftovers of the group */
    while(waitpid(-1,NULL,WNOHANG)>0);
    printf("R %d %.1f %s\n",code,now()-t0,line);
    if(code!=0){
      FILE*o=fopen("/tmp/opts.out","r");
      if(o){char b[300];int n=0;while(n<6&&fgets(b,sizeof b,o)){b[strcspn(b,"\n")]=0;printf("O %s\n",b);n++;}fclose(o);}
    }
  }
  printf("OPTS-DONE\n");
  return 0;
}

export class WebAudioPlayer {
  private audio: HTMLAudioElement | null = null;
  private currentFileId: string = '';
  private isEnded: boolean = false;
  private volume: number = 0.85;

  constructor() {
    if (typeof window !== 'undefined') {
      this.audio = new Audio();
      this.audio.preload = 'auto';
      this.audio.volume = this.volume;
      this.audio.onended = () => {
        this.isEnded = true;
      };
    }
  }

  play(fileId: string): void {
    if (!this.audio) return;
    if (this.currentFileId !== fileId) {
      this.currentFileId = fileId;
      this.audio.src = `/api/stream/${fileId}`;
      this.audio.load();
    }
    this.isEnded = false;
    this.audio.play().catch((err) => {
      console.warn('Web audio play error:', err);
    });
  }

  pause(): void {
    this.audio?.pause();
  }

  resume(): void {
    this.audio?.play().catch((err) => {
      console.warn('Web audio resume error:', err);
    });
  }

  stop(): void {
    if (!this.audio) return;
    this.audio.pause();
    this.audio.currentTime = 0;
    this.currentFileId = '';
    this.isEnded = false;
  }

  seek(seconds: number): void {
    if (!this.audio) return;
    this.audio.currentTime = seconds;
  }

  setVolume(vol: number): void {
    this.volume = Math.max(0, Math.min(1, vol));
    if (this.audio) {
      this.audio.volume = this.volume;
    }
  }

  getStatus() {
    return {
      fileId: this.currentFileId,
      currentTime: this.audio ? this.audio.currentTime : 0,
      duration: this.audio && !isNaN(this.audio.duration) ? this.audio.duration : 0,
      playing: this.audio ? !this.audio.paused && !this.audio.ended : false,
      ended: this.isEnded,
      error: '',
    };
  }
}

export const webAudio = new WebAudioPlayer();

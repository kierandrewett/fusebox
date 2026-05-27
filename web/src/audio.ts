let audio: HTMLAudioElement | null = null;

export function preloadSwitchSound() {
  if (audio) return;
  audio = new Audio("/assets/switch.wav");
  audio.preload = "auto";
}

export function playSwitchSound() {
  preloadSwitchSound();
  if (!audio) return;
  audio.currentTime = 0;
  void audio.play().catch(() => undefined);
}

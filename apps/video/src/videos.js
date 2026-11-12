import { LAUNCH } from './launch.js';
import { EXPLAINERS } from './explainers.js';

export const FPS = 30;
export const WIDTH = 1920;
export const HEIGHT = 1080;
export const LANGS = ['en', 'ja'];
export const VIDEOS = [LAUNCH, ...EXPLAINERS];

export const durationOf = (video) => video.scenes.reduce((sum, scene) => sum + scene.dur, 0);

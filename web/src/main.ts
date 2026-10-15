import { mount } from 'svelte';

import App from './App.svelte';
import './app.css';

const target = document.getElementById('app');
if (!target) {
  throw new Error('missing #app mount point');
}

// The vendored go2rtc player lives in static/ and is served at /; register
// it before any tile can mount one.
const player = (await import(/* @vite-ignore */ '/video-rtc.js')) as {
  VideoRTC: CustomElementConstructor;
};
customElements.define('video-rtc', player.VideoRTC);

mount(App, { target });

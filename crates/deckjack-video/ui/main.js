// The Video app's entry: the real kit, handed to the module that does the
// work, so a test can mount the same module with fakes.
import { bus, text } from "/kit/bus.js";
import { shell } from "/kit/shell.js";
import { mountVideo } from "./video.js";

mountVideo(document, { bus, shell, text });

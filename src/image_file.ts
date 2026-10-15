// 读本地图片文件：内容安全策略只放行 data: 图片（不放行 blob:），所以先读成 data URL 再解码。
export function loadImage(file: File): Promise<HTMLImageElement> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(new Error('图片读取失败'));
    reader.onload = () => {
      const picture = new Image();
      picture.onload = () => resolve(picture);
      picture.onerror = () => reject(new Error('这张图片无法解码'));
      picture.src = String(reader.result);
    };
    reader.readAsDataURL(file);
  });
}

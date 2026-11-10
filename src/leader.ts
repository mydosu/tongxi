/// 主脑在界面上的称呼：全局一个，用户可改（后端 leader_name，默认 Leader，已去掉 HTML 特殊字符）。
/// 提示词和落库的消息格式里仍叫「主脑」，这里只管给人看的文案。
let name = 'Leader';
export const setLeader = (value: string) => { name = value || 'Leader'; };
/// left / right：紧挨着的是不是中文；称呼是英文数字开头或结尾时在那一侧补个空格。
const cjk = (char?: string) => +(!!char && char >= '一' && char <= '鿿');
/// 后端报错这类现成的句子：里面的「主脑」换成称呼。
export const relabel = (text: string) => text.replace(/主脑/g, (_, at: number, all: string) => ld(cjk(all[at - 1]), cjk(all[at + 2])));
export const ld = (left = 0, right = 0) => (left && /^[!-~]/.test(name) ? ' ' : '') + name + (right && /[!-~]$/.test(name) ? ' ' : '');
